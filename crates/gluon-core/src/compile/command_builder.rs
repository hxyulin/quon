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
//! 9. feature cfgs, then the unit's cfg flags
//! 10. `--cap-lints`, panic strategy, the unit's raw rustc flags
//! 11. incremental directory, linker script, `-o` for raw binaries
//! 12. `-C extra-filename`

use super::compile_crate::CompileUnitInput;
use super::compile_utils::sanitise_crate_name;
use super::{CompileCtx, Emit, HOST_DEBUG_INFO, HOST_OPT_LEVEL, RustcCommandBuilder};
use crate::error::{Error, Result};
use crate::unit::{BuildUnit, DepTarget, Platform, UnitOrigin, UnitPaths, UnitProfile};
use gluon_model::{CrateType, TargetDef};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A ready-to-spawn rustc invocation for one unit.
pub(crate) struct UnitCommand {
    pub(crate) builder: RustcCommandBuilder,
    pub(crate) paths: UnitPaths,
    /// The path recorded in the cache and handed to dependents. The
    /// primary artifact, except under `gluon check` and `gluon clippy`,
    /// where crates (other than proc macros) write no artifact and the
    /// depfile stands in for it.
    pub(crate) output_path: PathBuf,
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
    let layout = &ctx.layout;
    let model = input.model;
    let resolved = input.resolved;
    let units = input.units;
    let artifacts = input.artifacts;
    let unit: &BuildUnit = units.get(input.unit);
    let paths = units.paths(input.unit, layout, model, resolved)?;

    // `gluon check` and `gluon clippy` swap the program and the emit
    // kinds. Vendored units are exempt (see `UnitSettings::always_rustc`).
    let driver = ctx.driver();
    let program = if unit.settings.always_rustc {
        ctx.rustc_info.rustc_path.clone()
    } else {
        driver.program(&ctx.rustc_info)
    };
    let mut builder = RustcCommandBuilder::new(&program);

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
        let config_extern_name = resolved
            .project
            .config_crate_name
            .clone()
            .unwrap_or_else(|| sanitise_crate_name(&resolved.project.name) + "_config");
        builder.extern_crate(&config_extern_name, config_path);
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
    let emit_override = if unit.kind == CrateType::ProcMacro || unit.settings.always_rustc {
        None
    } else {
        driver.emit_override()
    };
    builder.emit_with_dep_info_path(emit_override.unwrap_or(default_emit), &paths.depfile_path);

    let mut search_dirs: BTreeSet<PathBuf> = BTreeSet::new();
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
                }
                builder.extern_crate(&dep.extern_name, path);
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
    collect_transitive_dirs(input, unit, &mut search_dirs)?;
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
    if let UnitOrigin::Crate(h) = unit.origin {
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

    if unit.settings.incremental
        && let UnitOrigin::Crate(h) = unit.origin
        && let Some(def) = model.crates.get(h)
    {
        builder.incremental(&layout.incremental_dir(def));
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

    if let Some(extra) = &paths.extra_filename {
        builder
            .raw_arg("-C")
            .raw_arg(format!("extra-filename={extra}"));
    }

    let output_path = if emit_override.is_some() {
        paths.depfile_path.clone()
    } else {
        paths.output_path.clone()
    };
    Ok(UnitCommand {
        builder,
        paths,
        output_path,
    })
}

/// Add the directory of every unit reachable through `unit`'s deps, for
/// `-L dependency=<dir>`.
///
/// `--extern` covers only direct deps, but rustc also loads every crate
/// recorded in a direct dep's metadata by name, so it needs a search
/// path for the rest. Each unit has its own output directory, so this
/// collects one entry per transitive dep.
///
/// The walk does not descend into a proc macro's deps: those are linked
/// into the proc-macro dylib, and they are host units, whose directories
/// must not appear on a cross unit's search path.
fn collect_transitive_dirs(
    input: &CompileUnitInput<'_>,
    unit: &BuildUnit,
    dirs: &mut BTreeSet<PathBuf>,
) -> Result<()> {
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
        let path = input.artifacts.unit(id).ok_or_else(|| {
            Error::Compile(format!(
                "{} ran before its transitive dep {} finished (scheduler bug)",
                unit.name,
                units.get(id).name,
            ))
        })?;
        if let Some(parent) = path.parent() {
            dirs.insert(parent.to_path_buf());
        }
        let dep = units.get(id);
        if dep.kind != CrateType::ProcMacro {
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
    Ok(())
}
