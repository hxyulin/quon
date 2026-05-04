//! Per-crate compilation dispatch.
//!
//! [`compile`] is the single entry point for building a user crate (as
//! opposed to a sysroot crate handled by [`crate::sysroot`]). It mirrors
//! the cache integration pattern of `build_sysroot_crate` in
//! `sysroot/mod.rs` — read that function first if you are new to this
//! module.
//!
//! ### Host vs cross dispatch
//!
//! The `host` flag on [`ResolvedCrateRef`] determines which code path runs:
//!
//! - **Host crates** are compiled for the build machine. They get no
//!   `--sysroot`, no `--target`, and their outputs land in
//!   `<build>/host/<crate>/`.
//! - **Cross crates** are compiled for the profile's target. They get
//!   `--sysroot`, `--target`, implicit `--extern core/alloc/compiler_builtins`
//!   from the sysroot, and outputs under
//!   `<build>/cross/<target>/<profile>/<crate>/`.
//!
//! See `docs/src/architecture/cache.md` for the canonical rustc-flag
//! token order that determines the cache key hash.

use super::artifact_shape::CrateArtifactShape;
use super::command_builder::build_rustc_command;
#[cfg(test)]
use crate::cache::BuildRecord;
use crate::compile::layout::BuildLayout;
use crate::compile::rustc::RustcCommandBuilder;
use crate::compile::{ArtifactMap, CompileCtx};
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{BuildModel, CrateDef, ResolvedConfig, ResolvedCrateRef};
use std::path::{Path, PathBuf};

/// Input bundle for [`compile`]. Grouped into a struct so the function
/// signature stays legible as the number of inputs grows.
pub struct CompileCrateInput<'a> {
    /// The full build model (needed to resolve target/dep handles).
    pub model: &'a BuildModel,
    /// The resolved configuration for this build invocation.
    pub resolved: &'a ResolvedConfig,
    /// The specific crate to compile (with its resolved target binding).
    pub crate_ref: &'a ResolvedCrateRef,
    /// Map of already-built crate artifacts. The caller (scheduler) must
    /// have populated every dependency of `crate_ref` before calling.
    pub artifacts: &'a ArtifactMap,
    /// Path to the pre-built sysroot directory for the active target.
    /// **Required** for cross crates; ignored for host crates.
    pub sysroot_dir: Option<&'a Path>,
}

/// Walk a crate's `artifact_env` and return the list of referenced
/// artifact output paths. Used by [`compile`] to seed the cache
/// freshness query with artifact-dep mtimes, so that a kernel rebuild
/// (which updates the kernel binary's mtime but may leave its output
/// *path* unchanged — cross Bin crates have no extra-filename hash)
/// invalidates the consuming bootloader's cache.
///
/// Returns an empty vec for crates that don't use `artifact_env`.
///
/// Callers must have already built every referenced artifact; the
/// DAG edge from `artifact_deps` guarantees this at scheduler time.
/// Missing artifacts here are a scheduler bug.
pub(crate) fn collect_artifact_env_sources(
    crate_def: &CrateDef,
    model: &BuildModel,
    artifacts: &ArtifactMap,
) -> Result<Vec<PathBuf>> {
    let mut out = Vec::with_capacity(crate_def.artifact_env.len());
    for (env_key, dep_name) in &crate_def.artifact_env {
        let dep_handle = model.crates.lookup(dep_name).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' has artifact_env entry '{}={}', but no crate named '{}' \
                 exists in the build model",
                crate_def.name, env_key, dep_name, dep_name,
            ))
        })?;
        let artifact_path = artifacts.get(dep_handle).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}' artifact_env references '{}' but its artifact is not \
                 available in ArtifactMap (scheduler bug)",
                crate_def.name, dep_name,
            ))
        })?;
        out.push(artifact_path.to_path_buf());
    }
    Ok(out)
}

/// Everything `compile()` needs after argument resolution but before
/// spawning rustc. Built once by [`prepare_compile_context`]; consumed
/// by the [`crate::compile::run_compile_and_cache`] call at the tail
/// of `compile()`.
struct CompileContext {
    crate_name: String,
    /// `Some(target_name)` for cross crates; `None` for host crates.
    /// Captured here so the per-crate error closures don't need to
    /// re-resolve the target handle from the model.
    target_name_for_fail: Option<String>,
    is_host: bool,
    out_dir: PathBuf,
    output_path: PathBuf,
    depfile_path: PathBuf,
    cache_key: String,
    argv_hash: [u8; 32],
    builder: RustcCommandBuilder,
    extra_sources: Vec<PathBuf>,
}

/// Gather every input `compile()` needs after argument resolution but
/// before spawning rustc. Pulls together the rustc command builder,
/// cache key, output directory, and artifact-env source paths in one
/// place — leaving `compile()` itself as a thin wrapper that builds
/// error closures and dispatches to `run_compile_and_cache`.
fn prepare_compile_context(
    ctx: &CompileCtx,
    input: &CompileCrateInput<'_>,
) -> Result<CompileContext> {
    let model = input.model;
    let resolved = input.resolved;
    let crate_ref = input.crate_ref;

    let crate_def: &CrateDef = model.crates.get(crate_ref.handle).ok_or_else(|| {
        Error::Compile(format!(
            "crate handle {:?} not found in build model",
            crate_ref.handle
        ))
    })?;

    let (builder, output_path, depfile_path) = build_rustc_command(ctx, input)?;
    let argv_hash = builder.hash();
    let cache_key = build_cache_key(&ctx.layout, model, resolved, crate_def, crate_ref)?;

    // `out_dir` must exist on disk before rustc spawns; the
    // run_compile_and_cache helper takes care of the mkdir given
    // this path. Sourcing it from `CrateArtifactShape` keeps the
    // dispatch (host / cross-final / cross-artifact) in exactly
    // one place — the same place `command_builder` and
    // `artifact_path` consult.
    let shape = CrateArtifactShape::for_crate(&ctx.layout, model, resolved, crate_def, crate_ref)?;
    let out_dir = shape.out_dir;

    let target_name_for_fail = if crate_ref.host {
        None
    } else {
        Some(
            model
                .targets
                .get(crate_ref.target)
                .map(|t| t.name.clone())
                .unwrap_or_else(|| "<unknown>".to_string()),
        )
    };

    let extra_sources = collect_artifact_env_sources(crate_def, model, input.artifacts)?;

    Ok(CompileContext {
        crate_name: crate_def.name.clone(),
        target_name_for_fail,
        is_host: crate_ref.host,
        out_dir,
        output_path,
        depfile_path,
        cache_key,
        argv_hash,
        builder,
        extra_sources,
    })
}

/// Build the cache key for a single crate compilation.
///
/// The key shape — `"crate:host:<name>[:driver]"` for host crates and
/// `"crate:cross:<name>:<profile>:<target>[:driver]"` for cross-compiled
/// — is part of the on-disk cache contract: changing it invalidates all
/// existing cache entries. The `<driver>` suffix gives `gluon build`,
/// `gluon check`, and `gluon clippy` their own slots so they cannot
/// race over a single `BuildRecord`.
fn build_cache_key(
    layout: &BuildLayout,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    crate_def: &CrateDef,
    cref: &ResolvedCrateRef,
) -> Result<String> {
    let suffix = layout.cache_key_suffix();
    if cref.host {
        return Ok(format!("crate:host:{}{suffix}", crate_def.name));
    }
    let target = model.targets.get(cref.target).ok_or_else(|| {
        Error::Compile(format!(
            "crate '{}': target handle {:?} not found in build model",
            crate_def.name, cref.target
        ))
    })?;
    Ok(format!(
        "crate:cross:{}:{}:{}{}",
        crate_def.name, resolved.profile.name, target.name, suffix
    ))
}

/// Compile a single crate. Returns the absolute path of the produced
/// artifact and a boolean indicating whether the result came from the
/// cache (`true` = freshness check short-circuited rustc). Cached via
/// `ctx.cache` following the narrow-lock pattern established by
/// `sysroot::build_sysroot_crate`.
///
/// ### Cache contract
///
/// - Acquire cache lock → call `is_fresh` → drop immediately.
/// - If fresh and output exists: return early without spawning rustc.
/// - Else: spawn rustc, parse depfile, re-acquire lock → `mark_built` → drop.
///
/// **Never hold the cache lock across a rustc spawn** — doing so would
/// serialise parallel compilation on the cache mutex.
pub fn compile(
    ctx: &CompileCtx,
    input: CompileCrateInput<'_>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let cc = prepare_compile_context(ctx, &input)?;

    // Capture state into owned values so the error closures (which
    // outlive `cc`) can reference it. For cross crates we name the
    // target in the spawn-fail headline so multi-target builds don't
    // force the user to dig through the command to see which
    // invocation failed.
    let crate_name_fail = cc.crate_name.clone();
    let crate_name_depfile = cc.crate_name.clone();
    let is_host = cc.is_host;
    let target_name_for_fail = cc.target_name_for_fail.clone();
    let depfile_for_diag = cc.depfile_path.clone();

    crate::compile::run_compile_and_cache(
        ctx,
        cc.builder,
        cc.argv_hash,
        cc.cache_key,
        cc.output_path,
        cc.depfile_path,
        Some(&cc.out_dir),
        cc.extra_sources,
        Box::new(move |output, rendered| {
            let headline = if is_host {
                format!(
                    "rustc failed while building host crate '{}': exit={:?}",
                    crate_name_fail,
                    output.status.code(),
                )
            } else {
                format!(
                    "rustc failed while building crate '{}' for target '{}': exit={:?}",
                    crate_name_fail,
                    target_name_for_fail.as_deref().unwrap_or("<unknown>"),
                    output.status.code(),
                )
            };
            Error::Diagnostics(vec![
                Diagnostic::error(headline)
                    .with_note(format!(
                        "stderr:\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    ))
                    .with_note(format!("command: {rendered}")),
            ])
        }),
        Box::new(move |e| {
            Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "built crate '{}' but its depfile at {} could not be read",
                    crate_name_depfile,
                    depfile_for_diag.display(),
                ))
                .with_note(format!("underlying error: {e}"))
                .with_note(
                    "the artifact is on disk but the cache was not updated; \
                     the next run will re-spawn rustc for this crate",
                ),
            ])
        }),
        stderr_sink,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::compile::command_builder::build_rustc_command;
    use crate::compile::compile_utils::normalize_crate_name;
    use crate::compile::{ArtifactMap, BuildLayout, RustcCommandBuilder, RustcInfo};
    use gluon_model::{
        BuildModel, CrateDef, CrateType, DepDef, DepResolution, GroupDef, Handle, PanicStrategy,
        ProjectDef, ResolvedConfig, ResolvedCrateRef, ResolvedProfile, TargetDef, TargetSpec,
    };
    use std::borrow::Cow;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;

    // ---------------------------------------------------------------------------
    // Test fixtures
    // ---------------------------------------------------------------------------

    fn fake_rustc_info(rustc_path: impl Into<PathBuf>) -> RustcInfo {
        RustcInfo {
            rustc_path: rustc_path.into(),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test 2020-01-01)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: Some("deadbeef".into()),
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        }
    }

    /// Build a minimal `CompileCtx` backed by a fresh temp-dir cache.
    fn make_ctx(tmp: &Path, rustc_path: impl Into<PathBuf>) -> CompileCtx {
        make_ctx_with_driver(tmp, rustc_path, crate::compile::DriverKind::Rustc)
    }

    fn make_ctx_with_driver(
        tmp: &Path,
        rustc_path: impl Into<PathBuf>,
        driver: crate::compile::DriverKind,
    ) -> CompileCtx {
        let layout = BuildLayout::with_driver(tmp.join("build"), "testproject", driver);
        let info = fake_rustc_info(rustc_path);
        // Create the build dir before loading the cache so a later
        // cache.save() can atomically write-rename into it. Cache::load
        // tolerates a missing manifest file, so the order there doesn't
        // matter — but save() does not tolerate a missing parent dir.
        std::fs::create_dir_all(tmp.join("build")).unwrap();
        let cache = Cache::load(tmp.join("build/cache-manifest.json")).0;
        CompileCtx::new(layout, Arc::new(info), cache)
    }

    fn make_target(name: &str, spec: &str, builtin: bool) -> TargetDef {
        TargetDef {
            name: name.into(),
            spec: spec.into(),
            builtin,
            panic_strategy: None,
            span: None,
        }
    }

    fn make_target_with_panic(name: &str, spec: &str, strategy: PanicStrategy) -> TargetDef {
        TargetDef {
            name: name.into(),
            spec: spec.into(),
            builtin: true,
            panic_strategy: Some(strategy),
            span: None,
        }
    }

    fn make_profile(target: Handle<TargetDef>) -> ResolvedProfile {
        ResolvedProfile {
            name: "debug".into(),
            target,
            opt_level: 0,
            debug_info: false,
            lto: None,
            boot_binary: None,
            qemu_memory: None,
            qemu_cores: None,
            qemu_extra_args: Vec::new(),
            qemu_boot_mode: None,
            test_timeout: None,
        }
    }

    /// Insert a target into `model.targets` and return its handle.
    fn insert_target(model: &mut BuildModel, t: TargetDef) -> Handle<TargetDef> {
        let name = t.name.clone();
        let (h, _) = model.targets.insert(name, t);
        h
    }

    /// Insert a crate into `model.crates` and return its handle.
    ///
    /// Auto-creates a synthetic `host` group if `c.group` was not
    /// pre-set to a real handle. Tests can construct `CrateDef` with a
    /// trailing `..test_crate_defaults()` and rely on this helper to
    /// substitute a real group handle before insertion. Honest tests
    /// that build a real group + crate pair pass through unchanged.
    fn insert_crate(model: &mut BuildModel, mut c: CrateDef) -> Handle<CrateDef> {
        if model.groups.get(c.group).is_none() {
            c.group = ensure_default_group(model);
        }
        let name = c.name.clone();
        let (h, _) = model.crates.insert(name, c);
        h
    }

    /// Ensure a default `host` group exists in `model` and return its
    /// handle. Required for CrateDef construction, which now demands a
    /// non-optional `Handle<GroupDef>`.
    fn ensure_default_group(model: &mut BuildModel) -> Handle<GroupDef> {
        if let Some(h) = model.groups.lookup("host") {
            return h;
        }
        let (h, _) = model.groups.insert(
            "host".into(),
            GroupDef {
                name: "host".into(),
                target: TargetSpec::Host,
                ..Default::default()
            },
        );
        h
    }

    /// Default values for the newly-mandatory fields on `CrateDef`.
    /// Tests use `..test_crate_defaults()` instead of
    /// `..Default::default()` since `CrateDef` no longer derives
    /// `Default` (the new `group: Handle<GroupDef>` has no sensible
    /// default). The `group` field here is a placeholder dangling
    /// handle; `insert_crate` patches it to a real handle when the
    /// crate goes into the model.
    fn test_crate_defaults() -> CrateDef {
        CrateDef {
            name: String::new(),
            path: String::new(),
            edition: "2024".into(),
            crate_type: CrateType::Lib,
            target: TargetSpec::Host,
            deps: BTreeMap::new(),
            dev_deps: BTreeMap::new(),
            features: Vec::new(),
            root: None,
            linker_script: None,
            // Placeholder handle; insert_crate substitutes the real one.
            group: Handle::new(u32::MAX),
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
        }
    }

    /// Make a minimal `ResolvedConfig` for the given project root.
    fn make_resolved(target_handle: Handle<TargetDef>, project_root: PathBuf) -> ResolvedConfig {
        ResolvedConfig {
            project: ProjectDef {
                name: "testproject".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: make_profile(target_handle),
            options: Default::default(),
            crates: Vec::new(),
            build_dir: project_root.join("build"),
            project_root,
        }
    }

    /// Make a minimal host-crate `ResolvedCrateRef`.
    fn host_ref(handle: Handle<CrateDef>, target: Handle<TargetDef>) -> ResolvedCrateRef {
        ResolvedCrateRef {
            handle,
            target,
            host: true,
        }
    }

    /// Make a minimal cross-crate `ResolvedCrateRef`.
    fn cross_ref(handle: Handle<CrateDef>, target: Handle<TargetDef>) -> ResolvedCrateRef {
        ResolvedCrateRef {
            handle,
            target,
            host: false,
        }
    }

    fn args_as_strings(builder: &RustcCommandBuilder) -> Vec<String> {
        builder
            .args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    // ---------------------------------------------------------------------------
    // build_cache_key
    // ---------------------------------------------------------------------------

    #[test]
    fn build_cache_key_host_crate_uses_host_namespace() {
        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("host-tgt", "x86_64-unknown-linux-gnu", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "demo".into(),
                path: "demo".into(),
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, PathBuf::from("/tmp/proj"));
        let layout = BuildLayout::new("/tmp/build", "demo");
        let crate_def = model.crates.get(ch).unwrap();
        let cref = host_ref(ch, th);
        let key = build_cache_key(&layout, &model, &resolved, crate_def, &cref).unwrap();
        assert_eq!(key, "crate:host:demo");
    }

    #[test]
    fn build_cache_key_host_crate_with_check_driver_appends_suffix() {
        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("host-tgt", "x86_64-unknown-linux-gnu", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "demo".into(),
                path: "demo".into(),
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, PathBuf::from("/tmp/proj"));
        let layout =
            BuildLayout::with_driver("/tmp/build", "demo", crate::compile::DriverKind::Check);
        let crate_def = model.crates.get(ch).unwrap();
        let cref = host_ref(ch, th);
        let key = build_cache_key(&layout, &model, &resolved, crate_def, &cref).unwrap();
        assert_eq!(key, "crate:host:demo:check");
    }

    #[test]
    fn build_cache_key_cross_crate_includes_profile_and_target() {
        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("x86_64-unknown-none", "x86_64-unknown-none", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                target: TargetSpec::Cross(th),
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, PathBuf::from("/tmp/proj"));
        let layout = BuildLayout::new("/tmp/build", "demo");
        let crate_def = model.crates.get(ch).unwrap();
        let cref = cross_ref(ch, th);
        let key = build_cache_key(&layout, &model, &resolved, crate_def, &cref).unwrap();
        // Profile name comes from `make_profile` -> "debug".
        assert_eq!(key, "crate:cross:kernel:debug:x86_64-unknown-none");
    }

    #[test]
    fn build_cache_key_cross_crate_with_clippy_driver_appends_suffix() {
        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("x86_64-unknown-none", "x86_64-unknown-none", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                target: TargetSpec::Cross(th),
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, PathBuf::from("/tmp/proj"));
        let layout =
            BuildLayout::with_driver("/tmp/build", "demo", crate::compile::DriverKind::Clippy);
        let crate_def = model.crates.get(ch).unwrap();
        let cref = cross_ref(ch, th);
        let key = build_cache_key(&layout, &model, &resolved, crate_def, &cref).unwrap();
        assert_eq!(key, "crate:cross:kernel:debug:x86_64-unknown-none:clippy");
    }

    // ---------------------------------------------------------------------------
    // Flag assembly tests
    // ---------------------------------------------------------------------------

    // ---------------------------------------------------------------------------
    // normalize_crate_name
    // ---------------------------------------------------------------------------

    #[test]
    fn normalize_crate_name_passes_through_clean_names() {
        // No dash → borrowed (no allocation, identity).
        let n = normalize_crate_name("foo_bar");
        assert_eq!(n, "foo_bar");
        assert!(matches!(n, Cow::Borrowed(_)));
    }

    #[test]
    fn normalize_crate_name_replaces_dashes_with_underscores() {
        let n = normalize_crate_name("my-kernel-lib");
        assert_eq!(n, "my_kernel_lib");
        assert!(matches!(n, Cow::Owned(_)));
    }

    #[test]
    fn normalize_crate_name_leaves_other_invalid_chars_for_rustc_to_reject() {
        // Only `-` is normalized. `.` is left alone — rustc will reject it
        // loudly, which is the right outcome (the user typed something
        // genuinely wrong, not a stylistic dash).
        let n = normalize_crate_name("foo.bar");
        assert_eq!(n, "foo.bar");
    }

    #[test]
    fn flag_assembly_dash_named_crate_normalizes_for_rustc_and_filenames() {
        // End-to-end: a CrateDef named `my-lib` must reach rustc as
        // `--crate-name my_lib`, and the predicted output `.rlib` filename
        // must use the normalized name (since rustc derives the filename
        // from `--crate-name`).
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "my-lib".into(),
                path: "crates/my-lib".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, output_path, depfile_path) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        // The user-typed `my-lib` must NOT appear as the value of
        // `--crate-name` (rustc would reject it). It also must not appear
        // in `extra-filename`, since rustc embeds that into the actual
        // filename via `--crate-name`-derived rules.
        let crate_name_pos = args
            .iter()
            .position(|a| a == "--crate-name")
            .expect("--crate-name flag missing");
        assert_eq!(args[crate_name_pos + 1], "my_lib");

        // The predicted output filename must match what rustc will write:
        // `lib<NORMALIZED>-gluon-<NORMALIZED>.rlib`.
        let output_name = output_path.file_name().unwrap().to_str().unwrap();
        assert_eq!(output_name, "libmy_lib-gluon-my_lib.rlib");
        let dep_name = depfile_path.file_name().unwrap().to_str().unwrap();
        assert_eq!(dep_name, "my_lib-gluon-my_lib.d");
    }

    #[test]
    fn flag_assembly_host_crate_has_no_sysroot_or_target() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "mylib".into(),
                path: "crates/mylib".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        assert!(
            !args.iter().any(|a| a == "--sysroot"),
            "--sysroot must be absent for host crates, got: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--target")),
            "--target must be absent for host crates, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_cross_crate_has_sysroot_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        assert!(
            args.iter().any(|a| a == "--sysroot"),
            "--sysroot must be present for cross crates, got: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("--target")),
            "--target must be present for cross crates, got: {args:?}"
        );
        // Implicit sysroot crates.
        assert!(
            args.iter().any(|a| a.starts_with("core=")),
            "--extern core= must be present for cross crates, got: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("alloc=")),
            "--extern alloc= must be present for cross crates, got: {args:?}"
        );
        assert!(
            args.iter().any(|a| a.starts_with("compiler_builtins=")),
            "--extern compiler_builtins= must be present for cross crates, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_proc_macro_emits_proc_macro_crate_type() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("host", "x86_64-unknown-linux-gnu", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "mymacro".into(),
                path: "crates/mymacro".into(),
                edition: "2021".into(),
                crate_type: CrateType::ProcMacro,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        // --crate-type proc-macro is emitted as two tokens.
        let crate_type_pos = args.iter().position(|a| a == "--crate-type");
        assert!(
            crate_type_pos.is_some(),
            "--crate-type missing, got: {args:?}"
        );
        let next = args.get(crate_type_pos.unwrap() + 1);
        assert_eq!(
            next.map(|s| s.as_str()),
            Some("proc-macro"),
            "--crate-type must be proc-macro, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_respects_explicit_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        // Insert dep crate first so it gets handle 0.
        let dep_handle = insert_crate(
            &mut model,
            CrateDef {
                name: "myutil".into(),
                path: "crates/myutil".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );

        let mut deps = std::collections::BTreeMap::new();
        deps.insert(
            "myutil".to_string(),
            DepDef {
                resolution: DepResolution::Project(dep_handle),
                features: Vec::new(),
                version: None,
                optional: false,
                default_features: true,
                span: None,
            },
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..test_crate_defaults()
            },
        );

        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);

        let dep_rlib = PathBuf::from("/build/libmyutil-gluon-myutil.rlib");
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(dep_handle, dep_rlib.clone());

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        let extern_val = args
            .iter()
            .zip(args.iter().skip(1))
            .find(|(a, _)| a.as_str() == "--extern")
            .map(|(_, v)| v.as_str());
        assert_eq!(
            extern_val,
            Some("myutil=/build/libmyutil-gluon-myutil.rlib"),
            "--extern myutil=... must be present, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_injects_artifact_env_var() {
        // A consumer crate with `artifact_env = { KERNEL_PATH: "kernel" }`
        // must receive KERNEL_PATH=<kernel artifact path> in the rustc
        // command builder's env map. The referenced kernel crate's
        // output lives in the ArtifactMap.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));

        // Kernel crate (the artifact-dep target).
        let kernel_h = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..test_crate_defaults()
            },
        );

        // Bootloader crate with an artifact_env entry and a matching
        // artifact_deps ordering edge. Note: no regular `deps` entry —
        // this is pure artifact-level coupling, not a compile-time link.
        let mut artifact_env = std::collections::BTreeMap::new();
        artifact_env.insert("KERNEL_PATH".to_string(), "kernel".to_string());
        let bootloader_h = insert_crate(
            &mut model,
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                artifact_deps: vec!["kernel".into()],
                artifact_env,
                ..test_crate_defaults()
            },
        );

        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(bootloader_h, th);

        // The kernel's "artifact" — an arbitrary path. It does not have
        // to exist on disk for build_rustc_command; canonicalize falls
        // back to the original path when it can't resolve it.
        let kernel_out = PathBuf::from("/build/cross/x86/dev/final/kernel");
        let mut artifacts = ArtifactMap::new();
        artifacts.insert(kernel_h, kernel_out.clone());

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let envs = builder.envs();
        let kernel_env = envs
            .get(std::ffi::OsStr::new("KERNEL_PATH"))
            .expect("KERNEL_PATH must be injected");
        // canonicalize on /build/... will fall back to the literal.
        assert_eq!(
            kernel_env,
            kernel_out.as_os_str(),
            "KERNEL_PATH must carry the kernel artifact path"
        );
    }

    #[test]
    fn artifact_env_missing_artifact_returns_compile_error() {
        // If the referenced crate has no entry in ArtifactMap (scheduler
        // bug: artifact_deps edge was missing or the kernel hasn't built
        // yet), build_rustc_command must surface a clear error instead
        // of silently injecting garbage.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let _kernel_h = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                crate_type: CrateType::Bin,
                ..test_crate_defaults()
            },
        );
        let mut artifact_env = std::collections::BTreeMap::new();
        artifact_env.insert("KERNEL_PATH".to_string(), "kernel".to_string());
        let bootloader_h = insert_crate(
            &mut model,
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                artifact_deps: vec!["kernel".into()],
                artifact_env,
                ..test_crate_defaults()
            },
        );

        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(bootloader_h, th);
        let artifacts = ArtifactMap::new(); // empty — kernel not built

        let result = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        );

        let err = match result {
            Ok(_) => panic!("expected error when artifact_env target missing from ArtifactMap"),
            Err(e) => e,
        };
        let err_msg = format!("{err:?}");
        assert!(
            err_msg.contains("artifact_env"),
            "error must mention artifact_env: {err_msg}"
        );
    }

    #[test]
    fn missing_dep_in_artifact_map_returns_compile_error() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let dep_handle = insert_crate(
            &mut model,
            CrateDef {
                name: "missing_dep".into(),
                ..test_crate_defaults()
            },
        );

        let mut deps = std::collections::BTreeMap::new();
        deps.insert(
            "missing_dep".to_string(),
            DepDef {
                resolution: DepResolution::Project(dep_handle),
                features: Vec::new(),
                version: None,
                optional: false,
                default_features: true,
                span: None,
            },
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                deps,
                ..test_crate_defaults()
            },
        );

        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new(); // empty — dep not built

        let result = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        );

        match result {
            Err(Error::Compile(msg)) => {
                assert!(
                    msg.contains("missing_dep"),
                    "error message must name the missing dep, got: {msg}"
                );
            }
            Ok(_) => panic!("expected Err(Error::Compile), got Ok"),
            Err(e) => panic!("expected Err(Error::Compile), got Err({e})"),
        }
    }

    #[test]
    fn flag_assembly_cross_crate_passes_panic_strategy_when_set() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target_with_panic("x86", "x86_64-unknown-none", PanicStrategy::Abort),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        // Scan all -C <val> pairs to find panic=abort.
        let found_panic = args
            .windows(2)
            .any(|w| w[0] == "-C" && w[1] == "panic=abort");
        assert!(
            found_panic,
            "-C panic=abort must be present when panic_strategy=Some(\"abort\"), got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_cross_crate_omits_panic_strategy_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        let found_panic = args
            .windows(2)
            .any(|w| w[0] == "-C" && w[1].starts_with("panic="));
        assert!(
            !found_panic,
            "no panic= flag must be present when panic_strategy=None, got: {args:?}"
        );
    }

    #[test]
    fn flag_assembly_cross_lib_adds_config_crate_extern_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");
        let config_rlib = PathBuf::from("/build/libtestproject_config.rlib");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);

        let mut artifacts = ArtifactMap::new();
        artifacts.set_config_crate(th, config_rlib.clone());

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        // The config crate extern is: --extern testproject_config=<path>
        let found_config = args
            .windows(2)
            .any(|w| w[0] == "--extern" && w[1].starts_with("testproject_config="));
        assert!(
            found_config,
            "--extern testproject_config=... must be present when config_crate is set, got: {args:?}"
        );
    }

    #[test]
    fn cache_hit_skips_rustc_spawn() {
        // Arrange: pre-populate the cache with a matching BuildRecord and
        // write a zero-byte file at the expected output path. Point rustc at
        // a bogus path — any real spawn would return an I/O error, proving
        // the fast path was taken.
        //
        // The cache key and argv_hash are derived from the builder assembled
        // with a known rustc path. We pre-populate the manifest on disk,
        // then construct a fresh CompileCtx (pointing at a BOGUS rustc) that
        // loads the pre-populated manifest. If the cache hit is taken, compile
        // returns Ok without ever spawning the bogus rustc.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("build")).unwrap();

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "cached_crate".into(),
                path: "crates/cached_crate".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        // The bogus rustc path that the final ctx will use. We pre-compute
        // the argv_hash using this SAME path so the BuildRecord we write
        // matches what compile() will compute.
        let bogus_rustc = PathBuf::from("/dev/null/bogus-rustc");
        let manifest_path = tmp.path().join("build/cache-manifest.json");

        // Use a temporary ctx (same bogus path) just to compute the hash
        // and output/depfile paths — this ctx is discarded before calling
        // compile().
        let ctx_for_hash = make_ctx(tmp.path(), &bogus_rustc);
        let (builder, output_path, _depfile_path) = build_rustc_command(
            &ctx_for_hash,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();
        let argv_hash = builder.hash();
        drop(ctx_for_hash); // release the in-memory cache

        // Create the output file so output_path.exists() is true in is_fresh.
        std::fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        std::fs::write(&output_path, b"").unwrap();

        // Pre-populate the manifest on disk with a matching BuildRecord.
        // Sources is empty because no depfile exists → sources_for_query is
        // also empty, so the set-comparison in is_fresh passes trivially.
        let cache_key = "crate:host:cached_crate".to_string();
        {
            let mut cache = Cache::load(&manifest_path).0;
            cache
                .mark_built(BuildRecord {
                    key: cache_key,
                    argv_hash,
                    sources: Vec::new(),
                    output_path: output_path.clone(),
                })
                .expect("mark_built");
            cache.save().expect("save");
        }

        // Now build a FRESH ctx (same bogus rustc path so hash matches the
        // pre-populated record). This ctx loads the manifest we just wrote.
        let ctx = make_ctx(tmp.path(), &bogus_rustc);

        // compile() must hit the cache and return Ok(output_path) without
        // ever attempting to spawn /dev/null/bogus-rustc (which would fail
        // with an I/O error on any OS).
        let mut stderr_sink: Vec<u8> = Vec::new();
        let result = compile(
            &ctx,
            CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
            &mut stderr_sink,
        );
        let (got_path, was_cached) = result.unwrap();
        assert_eq!(got_path, output_path, "cache hit must return output_path");
        assert!(was_cached, "cache hit must report was_cached=true");
    }

    /// Regression guard: cross Bin crates must NOT emit `-C extra-filename` or
    /// `-o`, and the output path must end with just the crate name (no
    /// `-gluon-<name>` suffix). This exercises the special-case code in
    /// `build_rustc_command` that keeps cross binary names clean.
    #[test]
    fn flag_assembly_cross_bin_omits_extra_filename_and_uses_out_dir_in_final() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "hello".into(),
                path: "crates/hello".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, output_path, _depfile) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args: Vec<String> = builder
            .args()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-C" && w[1].contains("extra-filename")),
            "cross Bin must not emit -C extra-filename, got: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "-o"),
            "cross Bin must not emit -o, got: {args:?}"
        );

        // Output should be at <cross_final_dir>/hello, not <...>/hello-gluon-hello.
        assert!(
            output_path.ends_with("hello"),
            "output path should end with crate name, got: {output_path:?}"
        );
        assert!(
            !output_path.to_string_lossy().contains("-gluon-"),
            "output path should not contain -gluon- suffix, got: {output_path:?}"
        );
    }

    // ---------------------------------------------------------------
    // RawBinary crate-type tests
    // ---------------------------------------------------------------
    //
    // RawBinary is the flat-binary cousin of Bin. The invariants the
    // command builder must uphold:
    //
    //   1. Output filename ends with `.bin` (not the empty/exe suffix
    //      Bin would use).
    //   2. `-C link-arg=--oformat=binary` is appended so rust-lld
    //      writes a flat image instead of an ELF.
    //   3. `-o <output_path>` is set (rustc otherwise wouldn't write
    //      to a `.bin` extension).
    //   4. No `-C extra-filename` (same as Bin — rustc doesn't
    //      hash-suffix executables, and we want a stable filename).
    //   5. `--crate-type=bin` is what rustc sees (rustc has no
    //      first-class concept of flat-binary).
    //   6. Linker scripts are accepted — gating on Bin alone would
    //      reject them, breaking real-mode boot sectors which need
    //      `-T boot.ld` to define their load layout.
    #[test]
    fn flag_assembly_raw_binary_emits_oformat_and_dot_bin_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let sysroot_dir = tmp.path().join("sysroot");

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("x86-real", "x86_64-unknown-none", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "boot-sector".into(),
                path: "crates/boot-sector".into(),
                edition: "2021".into(),
                crate_type: CrateType::RawBinary,
                linker_script: Some("crates/boot-sector/boot.ld".into()),
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, output_path, _depfile) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot_dir),
            },
        )
        .unwrap();

        let args: Vec<String> = builder
            .args()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();

        // 1. Output filename uses `.bin`.
        assert!(
            output_path.to_string_lossy().ends_with("boot_sector.bin"),
            "RawBinary artifact must end with .bin (using rustc-normalized name), got: {output_path:?}"
        );

        // 2. `--oformat=binary` is injected as a link-arg.
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-C" && w[1] == "link-arg=--oformat=binary"),
            "RawBinary must emit -C link-arg=--oformat=binary, got: {args:?}"
        );

        // 3. `-o <output_path>` is set.
        let o_pos = args.iter().position(|a| a == "-o");
        assert!(
            o_pos.is_some(),
            "RawBinary must emit -o to force the .bin extension, got: {args:?}"
        );
        assert_eq!(
            args.get(o_pos.unwrap() + 1).map(String::as_str),
            Some(output_path.to_string_lossy().as_ref()),
            "-o must point at the resolved output path, got: {args:?}"
        );

        // 4. No `-C extra-filename`.
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-C" && w[1].contains("extra-filename")),
            "RawBinary must not emit -C extra-filename, got: {args:?}"
        );

        // 5. `--crate-type=bin` is what rustc sees (RawBinary has no
        //    native rustc representation).
        let ct_pos = args.iter().position(|a| a == "--crate-type").unwrap();
        assert_eq!(
            args.get(ct_pos + 1).map(String::as_str),
            Some("bin"),
            "RawBinary must pass --crate-type=bin to rustc, got: {args:?}"
        );

        // 6. Linker script is emitted (gated by is_executable, not
        //    crate_type == Bin).
        assert!(
            args.iter().any(|a| a.contains("boot.ld")),
            "RawBinary linker_script must be honoured, got: {args:?}"
        );
    }

    // ---------------------------------------------------------------
    // no_sysroot tests
    // ---------------------------------------------------------------
    //
    // A crate whose group sets `no_sysroot=true` must NOT receive:
    //   - `--sysroot <path>`
    //   - `--extern core=...`, `--extern alloc=...`,
    //     `--extern compiler_builtins=...`
    //   - `--extern <project>_config=...`
    //
    // It still gets `--target <spec>` (rustc needs a target to know
    // pointer width / atomic width / data layout etc.) and `--out-dir`.
    // sysroot_dir=None is the contract — the scheduler omits the
    // sysroot lookup for no_sysroot crates.
    #[test]
    fn flag_assembly_no_sysroot_group_omits_sysroot_and_implicit_externs() {
        use gluon_model::{GroupDef, TargetSpec};

        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("x86-real", "x86_64-unknown-none", true),
        );

        // Insert a no_sysroot group, then a crate that points at it.
        let (group_h, _) = model.groups.insert(
            "bios-boot".into(),
            GroupDef {
                name: "bios-boot".into(),
                target: TargetSpec::Cross(th),
                no_sysroot: true,
                ..Default::default()
            },
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "boot-sector".into(),
                path: "crates/boot-sector".into(),
                edition: "2021".into(),
                crate_type: CrateType::RawBinary,
                target: TargetSpec::Cross(th),
                group: group_h,
                ..test_crate_defaults()
            },
        );

        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = cross_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None, // scheduler passes None for no_sysroot crates
            },
        )
        .unwrap();

        let args: Vec<String> = builder
            .args()
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();

        // No --sysroot.
        assert!(
            !args.iter().any(|a| a == "--sysroot"),
            "no_sysroot crate must not emit --sysroot, got: {args:?}"
        );

        // No implicit `--extern core/alloc/compiler_builtins`.
        for forbidden in ["core", "alloc", "compiler_builtins"] {
            assert!(
                !args.iter().any(|a| a.starts_with(&format!("{forbidden}="))),
                "no_sysroot crate must not emit --extern {forbidden}=, got: {args:?}"
            );
        }

        // Target IS present (rustc needs it for pointer/atomic widths).
        // Builtin triples come as a single `--target=<spec>` token;
        // custom JSON specs come as two tokens (`--target` + path) so we
        // accept either form.
        assert!(
            args.iter()
                .any(|a| a == "--target" || a.starts_with("--target=")),
            "no_sysroot crate must still emit --target (target spec is mandatory \
             for cross compilation regardless of sysroot strategy), got: {args:?}"
        );
    }

    // ---------------------------------------------------------------
    // T2: DriverKind threading through compile_crate
    // ---------------------------------------------------------------

    #[test]
    fn driver_check_swaps_emit_to_metadata_only_and_keeps_rustc_path() {
        // DriverKind::Check should:
        //   1. Use the same rustc binary as DriverKind::Rustc.
        //   2. Force `--emit=metadata` regardless of crate type, even
        //      for libs that would normally also emit link.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx_with_driver(
            tmp.path(),
            "/usr/bin/rustc",
            crate::compile::DriverKind::Check,
        );

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("host", "x86_64", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "lib1".into(),
                path: "crates/lib1".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        // Program path is unchanged.
        assert_eq!(builder.rustc_path(), Path::new("/usr/bin/rustc"));

        // Emit kinds: must be metadata-only, no link, no dep-info.
        // Find the `--emit=...` argument and check its value.
        let args = args_as_strings(&builder);
        let emit_arg = args
            .iter()
            .find(|a| a.starts_with("--emit="))
            .expect("--emit flag missing");
        assert!(
            emit_arg.contains("metadata"),
            "expected metadata in {emit_arg}"
        );
        assert!(
            !emit_arg.contains("link"),
            "DriverKind::Check should suppress link emit, got {emit_arg}"
        );
    }

    #[test]
    fn driver_clippy_swaps_program_path() {
        // DriverKind::Clippy should resolve a clippy-driver path
        // (sibling-of-rustc heuristic in this test setup, since the
        // sentinel /usr/bin doesn't actually contain one), AND force
        // metadata-only emit. The exact resolved path depends on the
        // host environment; we assert the program is *not* the
        // configured rustc path, which is enough to prove the swap
        // happened.
        let tmp = tempfile::tempdir().unwrap();
        // Use a sentinel path that definitely has no clippy-driver
        // sibling so the resolver falls through to bare-name PATH
        // lookup. The sibling-of-rustc heuristic checks `is_file()` so
        // a non-existent sibling won't accidentally match.
        let ctx = make_ctx_with_driver(
            tmp.path(),
            "/nonexistent/path/rustc",
            crate::compile::DriverKind::Clippy,
        );

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("host", "x86_64", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "lib1".into(),
                path: "crates/lib1".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        // Program path should be clippy-driver (or .exe on Windows),
        // not the configured rustc path.
        let program = builder.rustc_path();
        let name = program.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name.starts_with("clippy-driver"),
            "expected clippy-driver, got {program:?}"
        );
        assert_ne!(program, Path::new("/nonexistent/path/rustc"));

        // Emit kinds should be metadata-only.
        let args = args_as_strings(&builder);
        let emit_arg = args
            .iter()
            .find(|a| a.starts_with("--emit="))
            .expect("--emit flag missing");
        assert!(emit_arg.contains("metadata"));
        assert!(!emit_arg.contains("link"));
    }

    #[test]
    fn driver_check_does_not_override_emit_for_proc_macros() {
        // Regression test: proc-macros must produce a dylib because the
        // dependent crate dlopens them at compile time. Forcing
        // metadata-only on a proc-macro would break the next crate's
        // build with "extern location for X does not exist".
        //
        // This is the same exemption cargo check uses — it always
        // builds proc-macros fully even when the rest of the workspace
        // is metadata-only.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx_with_driver(
            tmp.path(),
            "/usr/bin/rustc",
            crate::compile::DriverKind::Check,
        );

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("host", "x86_64", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "macro1".into(),
                path: "crates/macro1".into(),
                edition: "2021".into(),
                crate_type: CrateType::ProcMacro,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        let emit_arg = args.iter().find(|a| a.starts_with("--emit=")).unwrap();
        assert!(
            emit_arg.contains("link"),
            "proc-macro under DriverKind::Check must keep link emit, got {emit_arg}"
        );
    }

    #[test]
    fn driver_rustc_default_preserves_link_emit_for_libs() {
        // Sanity check: the historical default path must still emit
        // link for libraries. If this test fails, T2 has broken
        // gluon build.
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");

        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("host", "x86_64", true));
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "lib1".into(),
                path: "crates/lib1".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let (builder, _, _) = build_rustc_command(
            &ctx,
            &CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
        )
        .unwrap();

        let args = args_as_strings(&builder);
        let emit_arg = args.iter().find(|a| a.starts_with("--emit=")).unwrap();
        assert!(
            emit_arg.contains("link"),
            "DriverKind::Rustc must keep link emit, got {emit_arg}"
        );
    }

    /// End-to-end compilation of a trivial host lib crate against the real
    /// toolchain. Gated with `#[ignore]` because it spawns rustc — run with:
    ///   cargo test -p gluon-core -- --ignored e2e_compile_trivial_host_lib_crate
    #[test]
    #[ignore]
    fn e2e_compile_trivial_host_lib_crate() {
        let tmp = tempfile::tempdir().unwrap();

        // Write a trivial source file.
        let src_dir = tmp.path().join("crates/trivial/src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("lib.rs"), b"pub fn hi() {}").unwrap();

        let rustc_info = RustcInfo::probe().expect("probe rustc");
        let layout = BuildLayout::new(tmp.path().join("build"), "e2e");
        let cache = Cache::load(layout.cache_manifest()).0;
        std::fs::create_dir_all(layout.root()).unwrap();
        let ctx = CompileCtx::new(layout, Arc::new(rustc_info), cache);

        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("host", "x86_64-unknown-linux-gnu", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "trivial".into(),
                path: "crates/trivial".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, tmp.path().to_path_buf());
        let crate_ref = host_ref(ch, th);
        let artifacts = ArtifactMap::new();

        let mut stderr_sink: Vec<u8> = Vec::new();
        let result = compile(
            &ctx,
            CompileCrateInput {
                model: &model,
                resolved: &resolved,
                crate_ref: &crate_ref,
                artifacts: &artifacts,
                sysroot_dir: None,
            },
            &mut stderr_sink,
        );

        let (output_path, was_cached) = result.expect("compile should succeed");
        assert!(!was_cached, "first build must not report cached");
        assert!(output_path.exists(), "rlib must exist at {output_path:?}");
        assert!(
            std::fs::metadata(&output_path).unwrap().len() > 0,
            "rlib must be non-empty"
        );
    }
}
