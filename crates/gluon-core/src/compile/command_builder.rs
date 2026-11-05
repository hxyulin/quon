//! Rustc command assembly for one build unit.
//!
//! `build_unit_command` is the single place that turns a
//! [`BuildUnit`] into a rustc invocation, for crates declared in
//! `gluon.rs` and vendored crates alike. It reads only the unit, the
//! model and the outputs of finished nodes; it spawns nothing.
//! [`super::compile_crate::compile`] wraps it with the cache check and
//! the spawn.
//!
//! ### Argument order
//!
//! The argv is hashed into the cache key, so its order must be stable.
//! It is:
//!
//! 1. input, `--crate-name`, `--crate-type`, `--edition`
//! 2. `--sysroot`, `--target`, `--out-dir`
//! 3. implicit sysroot externs and the config crate extern
//! 4. `--emit`
//! 5. dep `--extern`s, in extern-name order
//! 6. `-L dependency=` directories, sorted
//! 7. `--extern proc_macro` for proc macros
//! 8. opt-level, debuginfo, LTO
//! 9. feature cfgs, then the unit's cfg flags, then for manifest units
//!    `--check-cfg` declarations and lint levels
//! 10. `--cap-lints`, panic strategy, the unit's raw rustc flags
//! 11. build-script output: `--cfg`, `--check-cfg`, `-l`, link args,
//!     then `-L` for the link-search paths of the unit's closure
//! 12. incremental directory, linker script, `-o` for raw binaries
//! 13. `-C metadata`, then `-C extra-filename`
//!
//! ### Build-script output
//!
//! A unit with a build-script run reads the run's parsed output from
//! the run node's result. Every part of it lands in the argv or the
//! child's env (`OUT_DIR`, `rustc-env`), so it is in the command hash:
//! a changed output reruns the unit, and a rerun that prints the same
//! directives does not.
//!
//! ### Search paths
//!
//! Units of one platform share a `deps/` directory and carry their
//! metadata hash in their file names, so one `-L dependency=` per
//! platform directory lets rustc find every transitive crate: it looks
//! them up by name and checks the hash recorded in the dependent. See
//! [`crate::unit::UnitGraph::search_dirs`].

use super::compile_crate::CompileUnitInput;
use super::{
    BuildLayout, CompileCtx, DriverKind, Emit, HOST_DEBUG_INFO, HOST_OPT_LEVEL, RustcCommandBuilder,
};
use crate::build_script::{LinkArg, LinkArgTarget};
use crate::compile::extern_map::ArtifactLookup;
use crate::error::{Error, Result};
use crate::unit::{
    BuildUnit, CargoSettings, DepTarget, Platform, UnitGraph, UnitId, UnitOrigin, UnitPaths,
    UnitProfile,
};
use gluon_model::{CrateType, TargetDef};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A ready-to-spawn rustc invocation for one unit.
pub(crate) struct UnitCommand {
    pub(crate) builder: RustcCommandBuilder,
    pub(crate) paths: UnitPaths,
    /// The path recorded in the cache and handed to dependents. The
    /// primary artifact, except under `gluon check` and `gluon clippy`,
    /// where libraries write only their `.rmeta` (which dependents
    /// load) and executables write nothing, so their depfile stands
    /// in.
    pub(crate) output_path: PathBuf,
    /// Files passed with `--extern` for direct deps and the config
    /// crate. They are declared freshness inputs: a dep rebuilt with
    /// different content reruns this unit, even where its path and so
    /// this command are unchanged (cross executables, prebuilt
    /// artifacts).
    pub(crate) dep_outputs: Vec<PathBuf>,
}

/// Assemble the rustc invocation for `input.unit`.
///
/// Every dependency must have finished: its output is read from
/// `input.artifacts`. A missing output is a scheduler bug and is
/// reported as one.
pub(crate) fn build_unit_command(
    ctx: &CompileCtx,
    input: &CompileUnitInput<'_>,
) -> Result<UnitCommand> {
    build_command(ctx, input, false)
}

/// Assemble the `gluon clippy` lint pass for a project unit that
/// [`build_unit_command`] compiles with plain rustc (see
/// [`needs_lint_pass`]): the same flags, run through clippy-driver with
/// metadata-only output into `<driver root>/host/lint/`. Nothing links
/// against that output; the pass exists for its diagnostics.
pub(crate) fn build_lint_command(
    ctx: &CompileCtx,
    input: &CompileUnitInput<'_>,
) -> Result<UnitCommand> {
    build_command(ctx, input, true)
}

/// Whether `gluon clippy` needs a separate lint pass for `unit`.
///
/// Project crates on the fixed host profile (proc macros and the host
/// crates they link) are compiled with plain rustc under every driver,
/// because dependents load the real dylib and rlibs. Compiling them with
/// clippy-driver instead would put two programs' records on one shared
/// output and make `gluon build` and `gluon clippy` rebuild each other's
/// copy. A second, metadata-only clippy-driver run keeps the shared
/// output and still reports the user's lints. Vendored units are not
/// linted: their lints are capped and the user cannot fix them.
pub(crate) fn needs_lint_pass(ctx: &CompileCtx, unit: &BuildUnit) -> bool {
    ctx.driver() == DriverKind::Clippy
        && unit.settings.always_rustc
        && matches!(unit.origin, UnitOrigin::Crate(_))
        && !unit.is_build_script
}

fn build_command(
    ctx: &CompileCtx,
    input: &CompileUnitInput<'_>,
    lint: bool,
) -> Result<UnitCommand> {
    let layout = &ctx.layout;
    let model = input.model;
    let resolved = input.resolved;
    let units = input.units;
    let artifacts = input.artifacts;
    let unit: &BuildUnit = units.get(input.unit);
    let mut paths = units.paths(input.unit, layout, model, resolved)?;
    if lint {
        paths = lint_paths(&paths, unit, layout);
    }

    // `gluon check` and `gluon clippy` swap the program and the emit
    // kinds. Vendored units are exempt (see `UnitSettings::always_rustc`).
    let driver = ctx.driver();
    let program = if unit.settings.always_rustc && !lint {
        ctx.rustc_info.rustc_path.clone()
    } else {
        driver.program(&ctx.rustc_info)
    };
    let mut builder = RustcCommandBuilder::new(&program);
    let mut dep_outputs = Vec::new();

    builder.input(&unit.src);
    builder
        .crate_name(&unit.crate_name)
        .crate_type(unit.kind)
        .edition(&unit.edition);

    let target: Option<&TargetDef> = match unit.platform {
        Platform::Host => None,
        Platform::Cross(t) => Some(model.targets.get(t).ok_or_else(|| {
            Error::Compile(format!(
                "unit '{}': target handle {t:?} not found in build model",
                unit.name
            ))
        })?),
    };

    if unit.sysroot {
        let sysroot = input.sysroot_dir.ok_or_else(|| {
            Error::Compile(format!(
                "unit '{}' links the sysroot but no sysroot_dir was provided \
                 (scheduler bug)",
                unit.name
            ))
        })?;
        builder.sysroot(sysroot);
    }
    if let Some(t) = target {
        // `no_sysroot` cross crates still get `--target`; they provide
        // their own runtime with `#![no_core]`.
        builder.target(&t.spec, t.builtin);
    }
    builder.out_dir(&paths.out_dir);

    if let (true, Some(t)) = (unit.sysroot, target) {
        // A custom sysroot means rustc does not inject these itself.
        // The file names match what `sysroot::build_sysroot_crate`
        // writes.
        let sysroot_lib = layout.sysroot_lib_dir(t);
        builder
            .extern_crate("core", &sysroot_lib.join("libcore-gluon-core.rlib"))
            .extern_crate(
                "compiler_builtins",
                &sysroot_lib.join("libcompiler_builtins-gluon-compiler_builtins.rlib"),
            )
            .extern_crate("alloc", &sysroot_lib.join("liballoc-gluon-alloc.rlib"));
    }
    if let (true, Platform::Cross(t)) = (unit.config_crate, unit.platform)
        && let Some(config_path) = artifacts.config_crate(t)
    {
        let config_extern_name = crate::generated::config_crate::config_crate_name(resolved);
        builder.extern_crate(&config_extern_name, config_path);
        dep_outputs.push(config_path.to_path_buf());
    }

    // Executables produce no metadata anyone links against. Proc macros
    // keep the full emit under check and clippy, because dependents
    // dlopen the dylib at compile time (`cargo check` does the same).
    // The depfile is always emitted: the cache needs it.
    let default_emit: &[Emit] = if unit.kind.is_executable() {
        &[Emit::Link, Emit::DepInfo]
    } else {
        &[Emit::Link, Emit::Metadata, Emit::DepInfo]
    };
    let emit_override = if lint {
        DriverKind::Clippy.emit_override()
    } else if unit.kind == CrateType::ProcMacro || unit.settings.always_rustc {
        None
    } else {
        driver.emit_override()
    };
    builder.emit_with_dep_info_path(emit_override.unwrap_or(default_emit), &paths.depfile_path);

    let mut search_dirs: BTreeSet<PathBuf> = units
        .search_dirs(input.unit, layout, model, resolved)?
        .into_iter()
        .collect();
    for dep in &unit.deps {
        match &dep.target {
            DepTarget::Unit(d) => {
                let path = artifacts.unit(*d).ok_or_else(|| {
                    Error::Compile(format!(
                        "{} depends on {} but its artifact is not available yet \
                         (scheduler bug: a dep must be built before its dependent)",
                        unit.name,
                        units.get(*d).name,
                    ))
                })?;
                builder.extern_crate(&dep.extern_name, path);
                dep_outputs.push(path.to_path_buf());
            }
            DepTarget::Prebuilt {
                path,
                search_dirs: dirs,
            } => {
                // Nightly cargo (1.101+) writes rlibs with only a
                // metadata stub and the real metadata in a sibling
                // `.rmeta`. rustc needs both under the same extern name,
                // as cargo passes them.
                let rmeta = path.with_extension("rmeta");
                if path.extension().is_some_and(|e| e == "rlib") && rmeta.is_file() {
                    builder.extern_crate(&dep.extern_name, &rmeta);
                    dep_outputs.push(rmeta);
                }
                builder.extern_crate(&dep.extern_name, path);
                dep_outputs.push(path.clone());
                search_dirs.extend(dirs.iter().cloned());
            }
            DepTarget::Missing { package } => {
                return Err(Error::Compile(format!(
                    "crate '{}' depends on external crate '{package}' but no vendor \
                     artifact is registered for it — run `gluon vendor` to build the \
                     external dep set",
                    unit.name,
                )));
            }
        }
    }
    collect_prebuilt_dirs(input, unit, &mut search_dirs);
    for dir in &search_dirs {
        builder.library_search_dependency(dir);
    }

    // Proc-macro source that imports `proc_macro` without an
    // `extern crate` line needs it passed explicitly. Bare `--extern`
    // makes rustc take it from its own sysroot, as cargo does.
    if unit.kind == CrateType::ProcMacro {
        builder.extern_crate_bare("proc_macro");
    }

    // `artifact_env`: the same resolution the analyzer uses, so `env!`
    // resolves identically under rustc and rust-analyzer.
    // A manifest build script has its owner's origin, not its env.
    if let UnitOrigin::Crate(h) = unit.origin
        && !unit.is_build_script
    {
        let def = model.crates.get(h).ok_or_else(|| {
            Error::Compile(format!("crate handle {h:?} not found in build model"))
        })?;
        let env_vars = crate::compile::env::resolve_crate_env(def, model, |dep| {
            artifacts.get(dep).map(Path::to_path_buf)
        })?;
        for (k, v) in &env_vars {
            builder.env(k.as_str(), v.as_str());
        }
    }

    match unit.profile {
        UnitProfile::Host => {
            builder
                .opt_level(HOST_OPT_LEVEL)
                .debug_info(HOST_DEBUG_INFO);
        }
        UnitProfile::Active => {
            builder
                .opt_level(resolved.profile.opt_level)
                .debug_info(resolved.profile.debug_info);
            if let Some(mode) = &resolved.profile.lto {
                builder.lto(mode);
            }
        }
    }

    for feat in &unit.features {
        builder.cfg(&format!("feature=\"{feat}\""));
    }
    for flag in &unit.settings.cfg_flags {
        builder.cfg(flag);
    }
    if let Some(cargo) = &unit.settings.cargo {
        let clippy = driver == DriverKind::Clippy && !unit.settings.always_rustc;
        push_cargo_settings(&mut builder, cargo, clippy);
    }
    if unit.settings.cap_lints {
        builder.raw_arg("--cap-lints").raw_arg("allow");
    }

    // Every crate for a target must use the sysroot's panic strategy,
    // or rustc rejects the link.
    if let Some(s) = target.and_then(|t| t.panic_strategy) {
        builder
            .raw_arg("-C")
            .raw_arg(format!("panic={}", s.as_str()));
    }

    for flag in &unit.settings.rustc_flags {
        builder.raw_arg(flag.as_str());
    }

    if let Some(run) = units.build_script_of(input.unit) {
        let out = artifacts.build_script(run).ok_or_else(|| {
            Error::Compile(format!(
                "{} ran before its build script finished (scheduler bug)",
                unit.name
            ))
        })?;
        let out_dir = units.run_dir(run, layout, model, resolved)?.join("out");
        builder.env("OUT_DIR", out_dir.as_os_str());
        for (k, v) in &out.env {
            builder.env(k.as_str(), v.as_str());
        }
        for cfg in &out.cfgs {
            builder.cfg(cfg);
        }
        for check in &out.check_cfgs {
            builder.raw_arg("--check-cfg").raw_arg(check.as_str());
        }
        for lib in &out.link_libs {
            builder.raw_arg("-l").raw_arg(lib.as_str());
        }
        for arg in out.link_args.iter().filter(|a| link_arg_applies(a, unit)) {
            builder
                .raw_arg("-C")
                .raw_arg(format!("link-arg={}", arg.arg));
        }
    }
    for path in closure_link_search(units, input.unit, artifacts) {
        builder.raw_arg("-L").raw_arg(path);
    }

    if let Some(dir) = &paths.incremental_dir {
        builder.incremental(dir);
    }

    if let Some(ls) = &unit.settings.linker_script {
        builder.linker_script(ls);
    }
    // rustc's default executable name has no extension; a raw binary
    // gets an explicit `<name>.elf` that the post-link objcopy step
    // turns into `<name>.bin`.
    if let Some(elf) = &paths.debug_elf_path {
        builder.raw_arg("-o").raw_arg(elf.as_os_str());
    }

    builder
        .raw_arg("-C")
        .raw_arg(format!("metadata={}", unit.metadata.hex()));
    if let Some(extra) = &paths.extra_filename {
        builder
            .raw_arg("-C")
            .raw_arg(format!("extra-filename={extra}"));
    }

    let output_path = match (emit_override, &paths.rmeta_path) {
        (None, _) => paths.output_path.clone(),
        (Some(_), Some(rmeta)) => rmeta.clone(),
        (Some(_), None) => paths.depfile_path.clone(),
    };
    Ok(UnitCommand {
        builder,
        paths,
        output_path,
        dep_outputs,
    })
}

/// Whether a build script's link argument applies to `unit`, as in
/// Cargo: libraries take none, `rustc-link-arg` applies to executables
/// and static libraries, `-bins` to executables, `-bin=NAME` to the
/// executable named `NAME`. Gluon builds no cdylibs.
fn link_arg_applies(arg: &LinkArg, unit: &BuildUnit) -> bool {
    match &arg.applies_to {
        LinkArgTarget::All => unit.kind.is_executable() || unit.kind == CrateType::StaticLib,
        LinkArgTarget::Bins => unit.kind.is_executable(),
        LinkArgTarget::Bin(name) => unit.kind.is_executable() && *name == unit.crate_name,
        LinkArgTarget::Cdylib => false,
    }
}

/// The `rustc-link-search` paths of the build scripts in `id`'s
/// closure: each direct dep's set (its result's
/// [`crate::scheduler::NodeData::LinkSearch`]), in dep order, then the
/// unit's own run's, without duplicates. Proc-macro deps are skipped:
/// their native libraries are linked into the dylib, on the host.
///
/// Each dep's set already covers its own closure, so this reads one
/// slot per direct dep and walks nothing.
pub(crate) fn closure_link_search(
    units: &UnitGraph,
    id: UnitId,
    artifacts: &dyn ArtifactLookup,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |paths: &[String]| {
        for p in paths {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    };
    for dep in &units.get(id).deps {
        if let DepTarget::Unit(d) = dep.target
            && units.get(d).kind != CrateType::ProcMacro
        {
            add(artifacts.link_search(d));
        }
    }
    if let Some(out) = units
        .build_script_of(id)
        .and_then(|r| artifacts.build_script(r))
    {
        add(&out.link_search);
    }
    out
}

/// Paths of the lint pass: metadata and depfile under
/// `<driver root>/host/lint/`, no incremental directory (rustc and
/// clippy-driver must not share one).
fn lint_paths(paths: &UnitPaths, unit: &BuildUnit, layout: &BuildLayout) -> UnitPaths {
    let out_dir = layout.lint_dir();
    let stem = format!("{}-{}", unit.crate_name, unit.metadata.short());
    UnitPaths {
        depfile_path: out_dir.join(format!("{stem}.d")),
        rmeta_path: Some(out_dir.join(format!("lib{stem}.rmeta"))),
        output_path: out_dir.join(format!("lib{stem}.rmeta")),
        debug_elf_path: None,
        extra_filename: paths.extra_filename.clone(),
        incremental_dir: None,
        out_dir,
    }
}

/// Append what Cargo passes for a manifest unit: `--check-cfg`
/// declarations, lint levels, and the `CARGO_*` env. All of it lands in
/// the argv or env, so the cache sees a change through the command
/// hash.
///
/// `clippy::` lints go only to `clippy-driver` (`clippy` is true when
/// the unit runs under it), the only driver that knows them;
/// `rustdoc::` lints go nowhere, because Gluon never runs rustdoc.
/// Cargo does the same.
fn push_cargo_settings(builder: &mut RustcCommandBuilder, cargo: &CargoSettings, clippy: bool) {
    for decl in &cargo.check_cfg {
        builder.raw_arg("--check-cfg").raw_arg(decl.as_str());
    }
    for lint in &cargo.lints {
        let name = match lint.tool.as_str() {
            "rust" => lint.name.clone(),
            "clippy" if clippy => format!("clippy::{}", lint.name),
            _ => continue,
        };
        let flag = match lint.level.as_str() {
            "allow" => "-A",
            "warn" => "-W",
            "deny" => "-D",
            "forbid" => "-F",
            // Manifest expansion rejects other levels.
            _ => continue,
        };
        builder.raw_arg(flag).raw_arg(name);
    }
    for (k, v) in &cargo.env {
        builder.env(k.as_str(), v.as_str());
    }
}

/// Add the search directories of every cargo-built prebuilt artifact
/// reachable through `unit`'s deps. Prebuilt artifacts live outside the
/// unit layout, so the platform `deps/` directories do not cover their
/// own deps.
///
/// The walk does not descend into a proc macro's deps: those are linked
/// into the proc-macro dylib, and they are host artifacts, whose
/// directories must not appear on a cross unit's search path.
fn collect_prebuilt_dirs(
    input: &CompileUnitInput<'_>,
    unit: &BuildUnit,
    dirs: &mut BTreeSet<PathBuf>,
) {
    let units = input.units;
    let mut seen = BTreeSet::new();
    let mut stack: Vec<_> = unit
        .deps
        .iter()
        .filter_map(|d| match d.target {
            DepTarget::Unit(u) => Some(u),
            _ => None,
        })
        .collect();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        let dep = units.get(id);
        if dep.kind == CrateType::ProcMacro {
            continue;
        }
        for d in &dep.deps {
            match &d.target {
                DepTarget::Unit(u) => stack.push(*u),
                DepTarget::Prebuilt {
                    search_dirs: more, ..
                } => dirs.extend(more.iter().cloned()),
                DepTarget::Missing { .. } => {}
            }
        }
    }
}
