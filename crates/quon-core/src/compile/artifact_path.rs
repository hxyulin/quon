//! Deterministic path computation for crate primary artifacts.
//!
//! Given a crate handle, returns the on-disk path where the crate's
//! primary build output *will* live (or already lives, for an
//! incremental build). The result is layout-derived and identical across
//! runs — it does not require the build to have happened.
//!
//! Both functions lower the unit table ([`crate::unit::lower`]) and read
//! the path from it, so they agree with the command builder by
//! construction. Lowering is pure and builds nothing.
//!
//! Vendored crates are not part of the lowered table here, because
//! building the vendor context runs `cargo metadata`. A declared library
//! that depends on a `Dependency` cross crate therefore gets a
//! different metadata hash here than in the build, and its predicted
//! path is wrong. Cross executables (the usual `artifact_env` target)
//! have no hash in their path and are unaffected.

use crate::compile::BuildLayout;
use crate::error::{Error, Result};
use crate::unit::{UnitPaths, lower};
use crate::vendor::cross::context::VendorBuildContext;
use quon_model::{BuildModel, CrateDef, Handle, ResolvedConfig};
use std::path::PathBuf;

/// `caller` names the public function in error messages.
fn crate_paths(
    caller: &str,
    layout: &BuildLayout,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    crate_handle: Handle<CrateDef>,
) -> Result<UnitPaths> {
    let crate_def = model.crates.get(crate_handle).ok_or_else(|| {
        Error::Compile(format!(
            "{caller}: handle {:?} not in build model",
            crate_handle
        ))
    })?;
    let units = lower(model, resolved, &VendorBuildContext::new())?;
    let unit = units.unit_of(crate_handle).ok_or_else(|| {
        Error::Compile(format!(
            "{caller}: crate '{}' is not in the resolved \
             crate set for the active profile (so its artifact path is not \
             well-defined for this build)",
            crate_def.name,
        ))
    })?;
    units.paths(unit, layout, model, resolved)
}

/// Compute the deterministic on-disk path of the primary artifact
/// produced by compiling the crate referenced by `crate_handle`.
///
/// `resolved.crates` must contain a [`quon_model::ResolvedCrateRef`]
/// for `crate_handle`; otherwise this returns `Err` (the crate is not
/// part of the current build, so its artifact path is undefined).
pub fn expected_crate_artifact_path(
    layout: &BuildLayout,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    crate_handle: Handle<CrateDef>,
) -> Result<PathBuf> {
    Ok(crate_paths(
        "expected_crate_artifact_path",
        layout,
        model,
        resolved,
        crate_handle,
    )?
    .output_path)
}

/// Returns `Some(<name>.elf)` for `CrateType::RawBinary`, `None` for
/// every other crate type.
///
/// Exists because `RawBinary` crates strip all ELF headers and debug
/// sections when the flat `<name>.bin` is produced — the `.elf` is the
/// debugger-facing sidecar that preserves symbols for `gdb`,
/// `addr2line`, `objdump`, and `readelf`.
pub fn expected_debug_elf_path(
    layout: &BuildLayout,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    crate_handle: Handle<CrateDef>,
) -> Result<Option<PathBuf>> {
    Ok(crate_paths(
        "expected_debug_elf_path",
        layout,
        model,
        resolved,
        crate_handle,
    )?
    .debug_elf_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quon_model::{
        BuildModel, CrateDef, CrateType, GroupDef, Handle, ProjectDef, ResolvedConfig,
        ResolvedCrateRef, ResolvedProfile, TargetDef, TargetSpec,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Ensure a "g" group exists in `model`, returning its handle.
    /// Required for CrateDef construction (group is now non-optional).
    fn ensure_group(model: &mut BuildModel) -> Handle<GroupDef> {
        if let Some(h) = model.groups.lookup("g") {
            return h;
        }
        let (h, _) = model.groups.insert(
            "g".into(),
            GroupDef {
                name: "g".into(),
                target: TargetSpec::Host,
                ..Default::default()
            },
        );
        h
    }

    /// Default values for newly-mandatory `CrateDef` fields. Tests use
    /// `..test_crate_defaults(group)` instead of `..Default::default()`.
    fn test_crate_defaults(group: Handle<GroupDef>) -> CrateDef {
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
            group,
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

    fn layout() -> BuildLayout {
        BuildLayout::new(PathBuf::from("/tmp/build"), "demo")
    }

    fn make_target(model: &mut BuildModel, name: &str, spec: &str) -> Handle<TargetDef> {
        let (h, _) = model.targets.insert(
            name.into(),
            TargetDef {
                name: name.into(),
                spec: spec.into(),
                builtin: true,
                build_std: false,
                panic_strategy: None,
                span: None,
            },
        );
        h
    }

    fn make_resolved(target: Handle<TargetDef>, crates: Vec<ResolvedCrateRef>) -> ResolvedConfig {
        ResolvedConfig {
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
                target: TargetSpec::Cross(target),
                opt_level: quon_model::OptLevel::Zero,
                debug_info: true,
                debug_assertions: true,
                overflow_checks: true,
                lto: None,
                run_binary: None,
                runner: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            },
            options: Default::default(),
            crates,
            build_dir: PathBuf::from("/tmp/build"),
            project_root: PathBuf::from("/tmp/proj"),
        }
    }

    #[test]
    fn cross_lib_artifact_lands_under_cross_deps_dir() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-unknown-none", "x86_64-unknown-none");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: TargetSpec::Cross(th),
                ..test_crate_defaults(g)
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let path = expected_crate_artifact_path(&layout(), &model, &resolved, ch).unwrap();
        let s = path.to_string_lossy().replace('\\', "/");
        let hash = s
            .strip_prefix("/tmp/build/cross/x86_64-unknown-none/dev/deps/libkernel-")
            .and_then(|r| r.strip_suffix(".rlib"))
            .unwrap_or_else(|| panic!("{s}"));
        assert_eq!(hash.len(), 16, "{s}");
    }

    #[test]
    fn cross_raw_binary_lands_under_cross_final_dir_with_bin_suffix() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "i386-code16", "i386-unknown-none-code16");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "boot-sector".into(),
            CrateDef {
                name: "boot-sector".into(),
                path: "boot-sector".into(),
                edition: "2024".into(),
                crate_type: CrateType::RawBinary,
                target: TargetSpec::Cross(th),
                ..test_crate_defaults(g)
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let path = expected_crate_artifact_path(&layout(), &model, &resolved, ch).unwrap();
        let s = path.to_string_lossy().replace('\\', "/");
        assert!(s.contains("/cross/i386-code16/dev/final/"), "{s}");
        // Cargo-style hyphenated names normalise to underscores for rustc.
        assert!(s.ends_with("/boot_sector.bin"), "{s}");
    }

    #[test]
    fn host_lib_artifact_uses_host_dir() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "host-target", "x86_64-unknown-linux-gnu");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "tooling".into(),
            CrateDef {
                name: "tooling".into(),
                path: "tools/tooling".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: TargetSpec::Host,
                ..test_crate_defaults(g)
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: true,
            }],
        );
        let path = expected_crate_artifact_path(&layout(), &model, &resolved, ch).unwrap();
        let s = path.to_string_lossy().replace('\\', "/");
        // A host lib no proc macro uses takes the active profile.
        assert!(
            s.starts_with("/tmp/build/host/profile/dev/deps/libtooling-"),
            "{s}"
        );
        assert!(s.ends_with(".rlib"), "{s}");
    }

    #[test]
    fn cross_raw_binary_exposes_debug_elf_sibling() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "i386-code16", "i386-unknown-none-code16");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "boot-sector".into(),
            CrateDef {
                name: "boot-sector".into(),
                path: "boot-sector".into(),
                edition: "2024".into(),
                crate_type: CrateType::RawBinary,
                target: TargetSpec::Cross(th),
                ..test_crate_defaults(g)
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let elf = expected_debug_elf_path(&layout(), &model, &resolved, ch).unwrap();
        let elf = elf.expect("RawBinary must expose debug_elf_path");
        let s = elf.to_string_lossy().replace('\\', "/");
        assert!(s.contains("/cross/i386-code16/dev/final/"), "{s}");
        assert!(s.ends_with("/boot_sector.elf"), "{s}");
    }

    #[test]
    fn cross_lib_has_no_debug_elf_sibling() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-unknown-none", "x86_64-unknown-none");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: TargetSpec::Cross(th),
                ..test_crate_defaults(g)
            },
        );
        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );
        let elf = expected_debug_elf_path(&layout(), &model, &resolved, ch).unwrap();
        assert!(
            elf.is_none(),
            "non-RawBinary crates must not expose a debug_elf_path"
        );
    }

    #[test]
    fn handle_not_in_resolved_crates_errors() {
        // Crate exists in the model but is not in resolved.crates (e.g. it
        // was filtered out by profile reachability). The function must
        // surface this as an error rather than silently fabricating a path.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "host-target", "x86_64-unknown-linux-gnu");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "orphan".into(),
            CrateDef {
                name: "orphan".into(),
                path: "orphan".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults(g)
            },
        );
        let resolved = make_resolved(th, Vec::new());
        let err = expected_crate_artifact_path(&layout(), &model, &resolved, ch);
        assert!(err.is_err());
    }

    #[test]
    fn debug_elf_handle_not_in_resolved_crates_errors() {
        // Same orphan-handle scenario as `handle_not_in_resolved_crates_errors`
        // but exercising the read-side `.elf` lookup. Confirms the error path
        // is wired and the function-name prefix in the error message
        // distinguishes this function from `expected_crate_artifact_path`.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "host-target", "x86_64-unknown-linux-gnu");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "orphan".into(),
            CrateDef {
                name: "orphan".into(),
                path: "orphan".into(),
                crate_type: CrateType::Lib,
                ..test_crate_defaults(g)
            },
        );
        let resolved = make_resolved(th, Vec::new());
        let err = expected_debug_elf_path(&layout(), &model, &resolved, ch);
        assert!(err.is_err());
        let msg = format!("{}", err.unwrap_err());
        assert!(
            msg.contains("expected_debug_elf_path:"),
            "error message must include the function-name prefix; got: {msg}"
        );
    }
}
