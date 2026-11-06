//! Compilation of one build unit.
//!
//! [`compile`] is the single entry point for building a crate unit,
//! whether it was declared in `gluon.rs` or comes from the vendor
//! pipeline (sysroot crates and the config crate have their own paths
//! in [`crate::sysroot`] and [`crate::generated::config_crate`]). It
//! assembles the command with
//! `command_builder::build_unit_command` and runs it through
//! `compile_and_cache`.
//!
//! See `docs/src/architecture/cache.md` for the freshness rules.

use super::command_builder::build_unit_command;
use crate::compile::extern_map::ArtifactLookup;
use crate::compile::{CompileCtx, PostCompileStep, RustcJob};
use crate::error::{Diagnostic, Error, Result};
use crate::unit::{Platform, UnitGraph, UnitId, UnitOrigin};
use gluon_model::{BuildModel, CrateDef, CrateType, ResolvedConfig};
use std::path::{Path, PathBuf};

/// Input bundle for [`compile`].
pub struct CompileUnitInput<'a> {
    pub model: &'a BuildModel,
    pub resolved: &'a ResolvedConfig,
    /// The unit table for this build.
    pub units: &'a UnitGraph,
    /// The unit to compile.
    pub unit: UnitId,
    /// Outputs of finished nodes. Every dep of `unit` must be present.
    pub artifacts: &'a dyn ArtifactLookup,
    /// The built sysroot for the unit's target. Required when the unit
    /// links the sysroot; ignored otherwise.
    pub sysroot_dir: Option<&'a Path>,
}

/// Walk a crate's `artifact_env` and return the referenced artifact
/// paths. They are extra freshness inputs: a kernel rebuild changes the
/// kernel binary but not its path (cross executables have no
/// extra-filename hash), so the consuming bootloader's argv alone would
/// miss it.
///
/// Callers must have built every referenced artifact; the DAG edge from
/// `artifact_deps` guarantees this. A missing artifact is a scheduler
/// bug.
pub(crate) fn collect_artifact_env_sources(
    crate_def: &CrateDef,
    model: &BuildModel,
    artifacts: &dyn ArtifactLookup,
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
                 available (scheduler bug)",
                crate_def.name, dep_name,
            ))
        })?;
        out.push(artifact_path.to_path_buf());
    }
    Ok(out)
}

/// Compile one unit. Returns the path dependents use and whether the
/// cache short-circuited rustc.
///
/// Freshness is checked and recorded by `compile_and_cache`. The
/// declared inputs are the direct deps' outputs and the `artifact_env`
/// artifacts.
/// The doctest command of a host-test lib test unit, or `None` for any
/// other unit, for a lib with `doctest = false`, and under `gluon check`
/// and `gluon clippy`, which build no test binaries.
fn doctest_command(
    ctx: &CompileCtx,
    input: &CompileUnitInput<'_>,
    builder: &super::RustcCommandBuilder,
) -> Result<Option<super::doctest::DoctestCommand>> {
    let unit = input.units.get(input.unit);
    if ctx.layout.driver() != super::DriverKind::Rustc || unit.kind != CrateType::Test {
        return Ok(None);
    }
    let UnitOrigin::Crate(h) = unit.origin else {
        return Ok(None);
    };
    let Some(def) = input.model.crates.get(h) else {
        return Ok(None);
    };
    let Some(lib_name) = def.cargo.as_ref().and_then(|c| c.doctest_lib.as_deref()) else {
        return Ok(None);
    };
    let lib = input
        .model
        .crates
        .lookup(lib_name)
        .and_then(|l| input.units.unit_of(l))
        .ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': its doctests link '{lib_name}', which is not in this build \
                 (lowering bug)",
                unit.name
            ))
        })?;
    let lib_path = input
        .units
        .paths(lib, &ctx.layout, input.model, input.resolved)?
        .output_path;
    let mut args = super::doctest::rustdoc_args(builder.args(), &unit.crate_name, &lib_path);
    // The doctests run from the package directory (see `run::host`).
    // A relative input names them `src/lib.rs - add (line 6)` instead
    // of by absolute path. Cargo runs rustdoc from the workspace root,
    // so its names start at the workspace root instead.
    let package_dir = input.resolved.project_root.join(&def.path);
    if let Some(first) = args.first_mut()
        && Path::new(first) == unit.src
        && let Ok(rel) = unit.src.strip_prefix(&package_dir)
    {
        *first = rel.as_os_str().to_os_string();
    }
    let exe = std::env::consts::EXE_SUFFIX;
    Ok(Some(super::doctest::DoctestCommand {
        program: ctx
            .rustc_info
            .sysroot
            .join("bin")
            .join(format!("rustdoc{exe}")),
        args,
        env: builder.envs().clone().into_iter().collect(),
    }))
}

pub fn compile(
    ctx: &CompileCtx,
    input: CompileUnitInput<'_>,
    stderr_sink: &mut Vec<u8>,
) -> Result<(PathBuf, bool)> {
    let model = input.model;
    let unit = input.units.get(input.unit);
    let cmd = build_unit_command(ctx, &input)?;
    let cache_key = input
        .units
        .cache_key(input.unit, &ctx.layout, model, input.resolved)?;

    let mut declared = cmd.dep_outputs.clone();
    declared.extend(match unit.origin {
        UnitOrigin::Crate(_) if unit.is_build_script => Vec::new(),
        UnitOrigin::Crate(h) => {
            let def = model.crates.get(h).ok_or_else(|| {
                Error::Compile(format!("crate handle {h:?} not found in build model"))
            })?;
            collect_artifact_env_sources(def, model, input.artifacts)?
        }
        UnitOrigin::Vendor(_) => Vec::new(),
    });
    let label = input.units.label(input.unit, model, input.resolved);

    // Owned copies for the error closures, which outlive `unit`.
    let script = if unit.is_build_script {
        "build script of "
    } else {
        ""
    };
    let what = match unit.origin {
        UnitOrigin::Crate(_) => format!("{script}crate '{}'", unit.name),
        UnitOrigin::Vendor(_) => format!("{script}vendored crate '{}'", unit.package.name),
    };
    let headline_what = match unit.platform {
        Platform::Host => match unit.origin {
            _ if unit.is_build_script => what.clone(),
            UnitOrigin::Crate(_) => format!("host crate '{}'", unit.name),
            UnitOrigin::Vendor(_) => format!("vendored crate '{}' for host", unit.package.name),
        },
        Platform::Cross(t) => {
            let target = model
                .targets
                .get(t)
                .map(|t| t.name.as_str())
                .unwrap_or("<unknown>");
            format!("{what} for target '{target}'")
        }
    };
    let depfile_for_diag = cmd.paths.depfile_path.clone();

    // A raw binary needs a post-link objcopy: rustc writes `<name>.elf`
    // and `rust-objcopy -O binary` produces `<name>.bin`, the primary
    // artifact. The `.elf` is also an aux output, so deleting it (or an
    // objcopy that crashed mid-run) forces a recompile instead of a
    // stale cache hit.
    let (aux_outputs, post_compile_step): (Vec<PathBuf>, Option<PostCompileStep>) =
        match (unit.kind, cmd.paths.debug_elf_path.clone()) {
            (CrateType::RawBinary, Some(elf_path)) => {
                let bin_path = cmd.output_path.clone();
                let rustc_path = ctx.rustc_info.rustc_path.clone();
                let rustlib_bin = crate::compile::objcopy::rustlib_bin_dir(
                    &ctx.rustc_info.sysroot,
                    &ctx.rustc_info.host_triple,
                );
                let elf_for_post = elf_path.clone();
                let post: PostCompileStep = Box::new(move || {
                    let objcopy =
                        crate::compile::objcopy::probe_rust_objcopy(&rustc_path, &rustlib_bin)?;
                    crate::compile::objcopy::run_objcopy_to_binary(
                        &objcopy,
                        &elf_for_post,
                        &bin_path,
                    )
                });
                (vec![elf_path], Some(post))
            }
            _ => (Vec::new(), None),
        };

    let lint = super::command_builder::needs_lint_pass(ctx, unit)
        .then(|| super::command_builder::build_lint_command(ctx, &input))
        .transpose()?;
    // Taken from the argv before the job consumes it; written once the
    // unit succeeds. See `compile::doctest`.
    let doctest = doctest_command(ctx, &input, &cmd.builder)?;

    let (output_path, was_cached) = crate::compile::run_compile_and_cache(
        ctx,
        RustcJob {
            what: label.clone(),
            builder: cmd.builder,
            key: cache_key.clone(),
            output_path: cmd.output_path,
            depfile_path: cmd.paths.depfile_path,
            out_dir: Some(&cmd.paths.out_dir),
            declared: declared.clone(),
            aux_outputs,
            post_compile_step,
            rustc_fail: rustc_fail_diag(headline_what.clone()),
            depfile_fail: depfile_fail_diag(what.clone(), depfile_for_diag),
        },
        stderr_sink,
    )?;
    if let Some(doctest) = doctest {
        super::doctest::write_command(&output_path, &doctest)?;
    }

    // The lint pass runs after the real compile, so a hard error is
    // reported once, by rustc, and the pass only adds clippy's lints.
    let Some(lint) = lint else {
        return Ok((output_path, was_cached));
    };
    let lint_depfile = lint.paths.depfile_path.clone();
    let (_, lint_cached) = crate::compile::run_compile_and_cache(
        ctx,
        RustcJob {
            what: format!("{label} (clippy lint pass)"),
            builder: lint.builder,
            key: format!("{cache_key}:clippy"),
            output_path: lint.output_path,
            depfile_path: lint.paths.depfile_path,
            out_dir: Some(&lint.paths.out_dir),
            declared,
            aux_outputs: Vec::new(),
            post_compile_step: None,
            rustc_fail: rustc_fail_diag(format!("{headline_what} (clippy lint pass)")),
            depfile_fail: depfile_fail_diag(what, lint_depfile),
        },
        stderr_sink,
    )?;
    Ok((output_path, was_cached && lint_cached))
}

fn rustc_fail_diag(headline_what: String) -> Box<crate::compile::compile_and_cache::RustcFailDiag> {
    Box::new(move |output| {
        Error::Diagnostics(vec![Diagnostic::error(format!(
            "rustc failed while building {headline_what}: exit={:?}",
            output.status.code(),
        ))])
    })
}

fn depfile_fail_diag(
    what: String,
    depfile: PathBuf,
) -> Box<crate::compile::compile_and_cache::DepfileFailDiag> {
    Box::new(move |e| {
        Error::Diagnostics(vec![
            Diagnostic::error(format!(
                "built {what} but its depfile at {} could not be read",
                depfile.display(),
            ))
            .with_note(format!("underlying error: {e}"))
            .with_note(
                "the artifact is on disk but the cache was not updated; \
                 the next run will re-spawn rustc for this crate",
            ),
        ])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::layout::BuildLayout;
    use crate::compile::{ArtifactMap, RustcCommandBuilder, RustcInfo};
    use crate::unit::lower;
    use crate::vendor::cross::context::VendorBuildContext;
    use gluon_model::{
        BuildModel, CrateDef, CrateType, DepDef, DepResolution, GroupDef, Handle, PanicStrategy,
        ProjectDef, ResolvedConfig, ResolvedCrateRef, ResolvedProfile, TargetDef, TargetSpec,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // ---------------------------------------------------------------------------
    // Shims: compile one declared crate the way the scheduler does, by
    // lowering it into a unit first.
    // ---------------------------------------------------------------------------

    /// One crate to compile, with its deps' artifacts recorded by crate
    /// handle.
    struct CompileCrateInput<'a> {
        model: &'a BuildModel,
        resolved: &'a ResolvedConfig,
        crate_ref: &'a ResolvedCrateRef,
        artifacts: &'a ArtifactMap,
        sysroot_dir: Option<&'a Path>,
    }

    /// Lower `crate_ref` plus its direct deps and every crate recorded
    /// in `artifacts`, and copy the recorded crate artifacts onto their
    /// units.
    fn lower_for(input: &CompileCrateInput<'_>) -> (ResolvedConfig, UnitGraph, ArtifactMap) {
        let model = input.model;
        let mut resolved = input.resolved.clone();
        resolved.crates.push(input.crate_ref.clone());
        let mut add = |h: Handle<CrateDef>| {
            if resolved.crates.iter().any(|r| r.handle == h) {
                return;
            }
            let (host, target) = match model.crates.get(h).map(|c| c.target) {
                Some(TargetSpec::Cross(t)) => (false, t),
                _ => (true, resolved.profile.target),
            };
            resolved.crates.push(ResolvedCrateRef {
                handle: h,
                target,
                host,
            });
        };
        if let Some(def) = model.crates.get(input.crate_ref.handle) {
            for dep in def.deps.values() {
                if let DepResolution::Project(h) = dep.resolution {
                    add(h);
                }
            }
        }
        for (h, _) in input.artifacts.iter() {
            add(h);
        }
        let units = lower(model, &resolved, &VendorBuildContext::new()).expect("lower");
        let mut artifacts = input.artifacts.clone();
        for (h, path) in input.artifacts.iter() {
            if let Some(u) = units.unit_of(h) {
                artifacts.insert_unit(u, path.to_path_buf());
            }
        }
        (resolved, units, artifacts)
    }

    fn build_rustc_command(
        ctx: &CompileCtx,
        input: &CompileCrateInput<'_>,
    ) -> Result<(RustcCommandBuilder, PathBuf, PathBuf)> {
        let (resolved, units, artifacts) = lower_for(input);
        let cmd = build_unit_command(
            ctx,
            &CompileUnitInput {
                model: input.model,
                resolved: &resolved,
                units: &units,
                unit: units.unit_of(input.crate_ref.handle).expect("unit"),
                artifacts: &artifacts,
                sysroot_dir: input.sysroot_dir,
            },
        )?;
        Ok((cmd.builder, cmd.output_path, cmd.paths.depfile_path))
    }

    fn compile(
        ctx: &CompileCtx,
        input: CompileCrateInput<'_>,
        stderr: &mut Vec<u8>,
    ) -> Result<(PathBuf, bool)> {
        let (resolved, units, artifacts) = lower_for(&input);
        super::compile(
            ctx,
            CompileUnitInput {
                model: input.model,
                resolved: &resolved,
                units: &units,
                unit: units.unit_of(input.crate_ref.handle).expect("unit"),
                artifacts: &artifacts,
                sysroot_dir: input.sysroot_dir,
            },
            stderr,
        )
    }

    fn build_cache_key(
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
        _crate_def: &CrateDef,
        cref: &ResolvedCrateRef,
    ) -> Result<String> {
        let artifacts = ArtifactMap::new();
        let (resolved, units, _) = lower_for(&CompileCrateInput {
            model,
            resolved,
            crate_ref: cref,
            artifacts: &artifacts,
            sysroot_dir: None,
        });
        units.cache_key(
            units.unit_of(cref.handle).unwrap(),
            layout,
            model,
            &resolved,
        )
    }

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
        std::fs::create_dir_all(tmp.join("build")).unwrap();
        CompileCtx::new(layout, Arc::new(info))
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
            run_binary: None,
            runner: None,
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
            cargo: None,
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
                crate_type: CrateType::ProcMacro,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, PathBuf::from("/tmp/proj"));
        let layout = BuildLayout::new("/tmp/build", "demo");
        let crate_def = model.crates.get(ch).unwrap();
        let cref = host_ref(ch, th);
        let key = build_cache_key(&layout, &model, &resolved, crate_def, &cref).unwrap();
        assert_eq!(mask_hash(&key), "crate:host:demo:H");
    }

    /// Replace the 16-hex-digit metadata hash in a cache key with `H`.
    fn mask_hash(key: &str) -> String {
        key.split(':')
            .map(|part| {
                if part.len() == 16 && part.chars().all(|c| c.is_ascii_hexdigit()) {
                    "H"
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join(":")
    }

    #[test]
    fn build_cache_key_host_proc_macro_is_shared_by_drivers() {
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
                crate_type: CrateType::ProcMacro,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, PathBuf::from("/tmp/proj"));
        let layout =
            BuildLayout::with_driver("/tmp/build", "demo", crate::compile::DriverKind::Check);
        let crate_def = model.crates.get(ch).unwrap();
        let cref = host_ref(ch, th);
        let key = build_cache_key(&layout, &model, &resolved, crate_def, &cref).unwrap();
        // Proc macros are built with plain rustc under every driver, so
        // `gluon check` reuses the record `gluon build` wrote.
        assert_eq!(mask_hash(&key), "crate:host:demo:H");
    }

    /// A host crate no proc macro depends on (a host tool) uses the
    /// active profile, so its key names the profile.
    #[test]
    fn build_cache_key_host_root_includes_profile() {
        let mut model = BuildModel::default();
        let th = insert_target(
            &mut model,
            make_target("host-tgt", "x86_64-unknown-linux-gnu", true),
        );
        let ch = insert_crate(
            &mut model,
            CrateDef {
                name: "mktool".into(),
                path: "mktool".into(),
                crate_type: CrateType::Bin,
                ..test_crate_defaults()
            },
        );
        let resolved = make_resolved(th, PathBuf::from("/tmp/proj"));
        let layout = BuildLayout::new("/tmp/build", "demo");
        let crate_def = model.crates.get(ch).unwrap();
        let key =
            build_cache_key(&layout, &model, &resolved, crate_def, &host_ref(ch, th)).unwrap();
        assert_eq!(mask_hash(&key), "crate:host:mktool:debug:H");
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
        assert_eq!(
            mask_hash(&key),
            "crate:cross:kernel:debug:x86_64-unknown-none:H"
        );
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
        assert_eq!(
            mask_hash(&key),
            "crate:cross:kernel:debug:x86_64-unknown-none:H:clippy"
        );
    }

    // ---------------------------------------------------------------------------
    // Flag assembly tests
    // ---------------------------------------------------------------------------

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
        // `lib<NORMALIZED><extra-filename>.rlib`.
        let extra = args
            .iter()
            .find_map(|a| a.strip_prefix("extra-filename="))
            .expect("extra-filename flag missing");
        let output_name = output_path.file_name().unwrap().to_str().unwrap();
        assert_eq!(output_name, format!("libmy_lib{extra}.rlib"));
        let dep_name = depfile_path.file_name().unwrap().to_str().unwrap();
        assert_eq!(dep_name, format!("my_lib{extra}.d"));
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
        let command = builder.hash();
        let cache_key = build_cache_key(
            &ctx_for_hash.layout,
            &model,
            &resolved,
            model.crates.get(ch).unwrap(),
            &crate_ref,
        )
        .unwrap();
        // Create the output file so the record's output exists.
        std::fs::create_dir_all(output_path.parent().unwrap()).unwrap();
        std::fs::write(&output_path, b"").unwrap();

        // Write a matching record. A host lib with no deps declares no
        // inputs, so the declared sets agree.
        ctx_for_hash
            .store
            .record(crate::cache::RecordInput {
                key: &cache_key,
                command,
                toolchain: ctx_for_hash.toolchain(),
                declared: &[],
                discovered: &[],
                env: BTreeMap::new(),
                outputs: std::slice::from_ref(&output_path),
                started_at: std::time::SystemTime::now(),
                stderr: &[],
                data: None,
            })
            .expect("record");
        drop(ctx_for_hash);

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
    /// metadata-hash suffix). This exercises the special-case code in
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

        // Output should be at <cross_final_dir>/hello, with no hash suffix.
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
    // RawBinary is the flat-binary cousin of Bin. The command builder
    // emits a normal ELF; a post-link objcopy step (wired in
    // compile_crate.rs via `post_compile_step`) converts it into the
    // flat `<name>.bin` the build model treats as the primary artifact.
    //
    // The invariants the command builder must uphold:
    //
    //   1. Build-model `output_path` stays `<name>.bin` — objcopy
    //      produces it; downstream consumers (image(), build_step())
    //      keep resolving to the same flat blob.
    //   2. `--oformat=binary` is GONE. rustc now produces a normal ELF
    //      so debuggers (gdb, addr2line, objdump) can consume it.
    //   3. `-o <name>.elf` is set explicitly — rustc's default
    //      executable name has no extension, and we want an unambiguous
    //      `.elf` suffix the post-link objcopy step can target.
    //   4. No `-C extra-filename` (same as Bin — rustc doesn't
    //      hash-suffix executables, and we want a stable filename).
    //   5. `--crate-type=bin` is what rustc sees (rustc has no
    //      first-class concept of flat-binary).
    //   6. Linker scripts are accepted — gating on Bin alone would
    //      reject them, breaking real-mode boot sectors which need
    //      `-T boot.ld` to define their load layout.
    #[test]
    fn flag_assembly_raw_binary_emits_dot_elf_output_no_oformat() {
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

        // After: rustc no longer produces the flat `.bin`. It emits a
        // standard ELF named `<name>.elf` into cross_final_dir, and a
        // post-compile objcopy step (Task 7) converts it to `<name>.bin`.
        //
        // What this test pins:
        //   1. Output path is still `<name>.bin` (the build model treats
        //      it as the primary artifact; objcopy produces it).
        //   2. `--oformat=binary` is GONE from the rustc invocation.
        //   3. `-o <name>.elf` is set explicitly so the rustc-produced
        //      ELF has an unambiguous extension.
        //   4. No `-C extra-filename` (unchanged).
        //   5. `--crate-type=bin` is what rustc sees (unchanged).
        //   6. Linker scripts still accepted (unchanged).

        // 1. Build-model primary artifact path is unchanged: `.bin`.
        assert!(
            output_path.to_string_lossy().ends_with("boot_sector.bin"),
            "build-model output_path stays .bin (objcopy produces it), got: {output_path:?}"
        );

        // 2. `--oformat=binary` must NOT appear anywhere.
        assert!(
            !args.iter().any(|a| a.contains("--oformat=binary")),
            "RawBinary must NOT pass --oformat=binary anymore, got: {args:?}"
        );

        // 3. `-o <something>.elf` is set.
        let o_pos = args
            .iter()
            .position(|a| a == "-o")
            .expect("RawBinary must emit -o <name>.elf");
        let o_path = &args[o_pos + 1];
        assert!(
            o_path.ends_with(".elf"),
            "-o argument must end in .elf, got: {o_path}"
        );
        assert!(
            !o_path.ends_with(".bin"),
            "-o must no longer pass .bin to rustc directly, got: {o_path}"
        );

        // 4. No `-C extra-filename`.
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "-C" && w[1].contains("extra-filename")),
            "RawBinary must not emit -C extra-filename, got: {args:?}"
        );

        // 5. `--crate-type=bin`.
        let ct_pos = args.iter().position(|a| a == "--crate-type").unwrap();
        assert_eq!(
            args.get(ct_pos + 1).map(String::as_str),
            Some("bin"),
            "RawBinary must pass --crate-type=bin to rustc, got: {args:?}"
        );

        // 6. Linker script still emitted.
        assert!(
            args.iter().any(|a| a.contains("boot.ld")),
            "RawBinary linker_script must still be honoured, got: {args:?}"
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

        let (builder, output_path, _) = build_rustc_command(
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

        // Dependents link against the `.rmeta`, the only file a checked
        // library writes. Handing them the depfile breaks every
        // library-to-library dependency under `gluon check`.
        let extra = args
            .iter()
            .find_map(|a| a.strip_prefix("extra-filename="))
            .expect("extra-filename flag missing");
        assert_eq!(
            output_path,
            tmp.path()
                .join("build/tool/check/host/profile/debug/deps")
                .join(format!("liblib1{extra}.rmeta"))
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
        std::fs::create_dir_all(layout.root()).unwrap();
        let ctx = CompileCtx::new(layout, Arc::new(rustc_info));

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

    /// A rustc upgrade at the same path (same argv) must rebuild user
    /// crates. Gated with `#[ignore]` because it spawns rustc.
    #[test]
    #[ignore]
    fn e2e_rustc_version_change_rebuilds_host_crate() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("crates/trivial/src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("lib.rs"), b"pub fn hi() {}").unwrap();

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
        let layout = BuildLayout::new(tmp.path().join("build"), "e2e");
        std::fs::create_dir_all(layout.root()).unwrap();

        // Each run reads and writes the records on disk with the given
        // toolchain identity.
        let run = |info: RustcInfo| -> bool {
            let ctx = CompileCtx::new(layout.clone(), Arc::new(info));
            let mut stderr_sink = Vec::new();
            let (_, was_cached) = compile(
                &ctx,
                CompileCrateInput {
                    model: &model,
                    resolved: &resolved,
                    crate_ref: &crate_ref,
                    artifacts: &artifacts,
                    sysroot_dir: None,
                },
                &mut stderr_sink,
            )
            .expect("compile should succeed");
            was_cached
        };

        let info = RustcInfo::probe().expect("probe rustc");
        assert!(!run(info.clone()), "cold build must compile");
        assert!(run(info.clone()), "same toolchain must hit the cache");

        let mut upgraded = info;
        upgraded.version.push_str(" (upgraded)");
        assert!(
            !run(upgraded),
            "a different rustc version at the same path must rebuild"
        );
    }

    // ---------------------------------------------------------------------------
    // Vendored units go through the same command builder
    // ---------------------------------------------------------------------------

    fn vendor_unit(
        name: &str,
        target: TargetSpec,
        kind: crate::vendor::cross::metadata::CrateKind,
        deps: &[(&str, TargetSpec)],
    ) -> crate::vendor::cross::graph::VendorCrateBuild {
        use crate::vendor::cross::graph::{VendorCrateBuild, VendorDepEdge};
        use crate::vendor::cross::metadata::CrateId;
        VendorCrateBuild {
            crate_id: CrateId {
                name: name.into(),
                version: "1.0.0".into(),
            },
            cargo_target_name: name.into(),
            target,
            kind,
            manifest_dir: PathBuf::from(format!("/vendor/{name}")),
            lib_path: PathBuf::from(format!("/vendor/{name}/src/lib.rs")),
            edition: "2021".into(),
            features: vec!["std".into()],
            deps: deps
                .iter()
                .map(|(dep, target)| VendorDepEdge {
                    extern_name: dep.to_string(),
                    crate_id: CrateId {
                        name: dep.to_string(),
                        version: "1.0.0".into(),
                    },
                    target: *target,
                })
                .collect(),
            build_script: None,
        }
    }

    /// Build the command for one vendored unit, with every other unit
    /// "built" at `/built/<unit name>/artifact`.
    fn vendor_command(
        ctx: &CompileCtx,
        model: &BuildModel,
        resolved: &ResolvedConfig,
        vctx: &VendorBuildContext,
        which: crate::vendor::cross::context::VendorBuildId,
    ) -> (Vec<String>, PathBuf, PathBuf, String) {
        let units = lower(model, resolved, vctx).unwrap();
        let mut artifacts = ArtifactMap::new();
        for (id, unit) in units.iter() {
            artifacts.insert_unit(id, PathBuf::from(format!("/built/{}/artifact", unit.name)));
        }
        let sysroot = PathBuf::from("/sysroot");
        let cmd = build_unit_command(
            ctx,
            &CompileUnitInput {
                model,
                resolved,
                units: &units,
                unit: units.unit_of_vendor(which).unwrap(),
                artifacts: &artifacts,
                sysroot_dir: Some(&sysroot),
            },
        )
        .unwrap();
        (
            args_as_strings(&cmd.builder),
            cmd.output_path,
            cmd.builder.rustc_path().to_path_buf(),
            units
                .get(units.unit_of_vendor(which).unwrap())
                .metadata
                .short(),
        )
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn vendored_host_proc_macro_uses_host_profile_and_plain_rustc() {
        use crate::vendor::cross::metadata::CrateKind;
        let tmp = tempfile::tempdir().unwrap();
        // Under `gluon check`, vendored units still get plain rustc and a
        // full emit: dependents need the real dylib.
        let ctx = make_ctx_with_driver(
            tmp.path(),
            "/usr/bin/rustc",
            crate::compile::DriverKind::Clippy,
        );
        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let mut resolved = make_resolved(th, tmp.path().to_path_buf());
        resolved.profile.opt_level = 3;
        let mut vctx = VendorBuildContext::new();
        let syn = vctx
            .add(vendor_unit(
                "syn",
                TargetSpec::Host,
                CrateKind::ProcMacro,
                &[],
            ))
            .unwrap();

        let (args, output, program, h16) = vendor_command(&ctx, &model, &resolved, &vctx, syn);
        assert_eq!(program, PathBuf::from("/usr/bin/rustc"));
        assert!(
            !args.iter().any(|a| a == "--target" || a == "--sysroot"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "opt-level=0"), "{args:?}");
        assert!(args.iter().any(|a| a == "debuginfo=0"), "{args:?}");
        assert!(has_pair(&args, "--cap-lints", "allow"), "{args:?}");
        assert!(has_pair(&args, "--cfg", "feature=\"std\""), "{args:?}");
        assert!(has_pair(&args, "--extern", "proc_macro"), "{args:?}");
        assert!(
            args.iter().any(|a| a.starts_with("--emit=link")),
            "{args:?}"
        );
        // Shared with `gluon build`: the build root, not `tool/clippy/`.
        assert_eq!(
            output,
            tmp.path().join("build/host/deps").join(
                crate::compile::compile_utils::proc_macro_file_name(&format!("syn-{h16}"))
            )
        );
    }

    #[test]
    fn vendored_cross_lib_links_sysroot_deps_and_transitive_dirs() {
        use crate::vendor::cross::metadata::CrateKind;
        let tmp = tempfile::tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), "/usr/bin/rustc");
        let mut model = BuildModel::default();
        let th = insert_target(&mut model, make_target("x86", "x86_64-unknown-none", true));
        let mut resolved = make_resolved(th, tmp.path().to_path_buf());
        resolved.profile.opt_level = 3;
        let cross = TargetSpec::Cross(th);
        let host = TargetSpec::Host;
        // top → a → b (cross libs); top → derive (host proc macro) → quote.
        let mut vctx = VendorBuildContext::new();
        vctx.add(vendor_unit("b", cross, CrateKind::Lib, &[]))
            .unwrap();
        vctx.add(vendor_unit("a", cross, CrateKind::Lib, &[("b", cross)]))
            .unwrap();
        vctx.add(vendor_unit("quote", host, CrateKind::Lib, &[]))
            .unwrap();
        vctx.add(vendor_unit(
            "derive",
            host,
            CrateKind::ProcMacro,
            &[("quote", host)],
        ))
        .unwrap();
        let top = vctx
            .add(vendor_unit(
                "top",
                cross,
                CrateKind::Lib,
                &[("a", cross), ("derive", host)],
            ))
            .unwrap();

        let (args, output, _, h16) = vendor_command(&ctx, &model, &resolved, &vctx, top);
        assert!(
            args.iter().any(|a| a == "--target=x86_64-unknown-none"),
            "{args:?}"
        );
        assert!(has_pair(&args, "--sysroot", "/sysroot"), "{args:?}");
        assert!(args.iter().any(|a| a == "opt-level=3"), "{args:?}");
        assert!(
            has_pair(&args, "--extern", "a=/built/a/artifact"),
            "{args:?}"
        );
        assert!(
            has_pair(&args, "--extern", "derive=/built/derive/artifact"),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|a| a.contains("_config")),
            "vendored crates do not link the config crate: {args:?}"
        );
        // Transitive deps are found through the platform `deps/`
        // directories: one for the target, one for the host.
        let search: Vec<String> = args
            .windows(2)
            .filter(|w| w[0] == "-L")
            .map(|w| w[1].clone())
            .collect();
        let build = tmp.path().join("build");
        assert_eq!(
            search,
            vec![
                format!(
                    "dependency={}",
                    build.join("cross/x86/debug/deps").display()
                ),
                format!("dependency={}", build.join("host/deps").display()),
            ]
        );
        assert!(args.iter().any(|a| a.starts_with("metadata=")), "{args:?}");
        assert_eq!(
            output,
            build.join(format!("cross/x86/debug/deps/libtop-{h16}.rlib"))
        );
    }
}
