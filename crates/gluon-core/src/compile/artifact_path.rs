//! Deterministic path computation for crate primary artifacts.
//!
//! Given a crate handle, returns the on-disk path where the crate's
//! primary build output *will* live (or already lives, for an
//! incremental build). The result is layout-derived and identical across
//! runs — it does not require the build to have happened.
//!
//! This is the read-only inverse of the path arithmetic that
//! [`crate::compile::command_builder::build_rustc_command`] performs
//! when assembling the rustc invocation. The two implementations must
//! stay in lockstep — if you change one, you must change the other, or
//! analyzer-time and build-time consumers will see different paths for
//! the same crate.
//!
//! The primary consumer today is [`crate::analyzer`], which needs to
//! emit `env!("KERNEL_PATH") → /path/to/kernel` mappings into
//! `rust-project.json` *before* any build has been kicked off, so that
//! `env!()` macros resolve under rust-analyzer to the same path rustc
//! will eventually use.

use crate::compile::BuildLayout;
use crate::compile::compile_utils::{exe_suffix_for_target, normalize_crate_name};
use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateDef, CrateType, Handle, ResolvedConfig};
use std::path::PathBuf;

/// Compute the deterministic on-disk path of the primary artifact
/// produced by compiling the crate referenced by `crate_handle`.
///
/// `resolved.crates` must contain a [`gluon_model::ResolvedCrateRef`]
/// for `crate_handle`; otherwise this returns `Err` (the crate is not
/// part of the current build, so its artifact path is undefined).
pub fn expected_crate_artifact_path(
    layout: &BuildLayout,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    crate_handle: Handle<CrateDef>,
) -> Result<PathBuf> {
    let crate_def = model.crates.get(crate_handle).ok_or_else(|| {
        Error::Compile(format!(
            "expected_crate_artifact_path: handle {:?} not in build model",
            crate_handle
        ))
    })?;

    let cref = resolved
        .crates
        .iter()
        .find(|r| r.handle == crate_handle)
        .ok_or_else(|| {
            Error::Compile(format!(
                "expected_crate_artifact_path: crate '{}' is not in the resolved \
                 crate set for the active profile (so its artifact path is not \
                 well-defined for this build)",
                crate_def.name,
            ))
        })?;

    let crate_name_rustc = normalize_crate_name(&crate_def.name);
    let extra_filename = format!("-gluon-{crate_name_rustc}");

    if cref.host {
        // Mirror command_builder.rs:81-113. Host crates land under
        // `<root>/host/<crate>/`. The artifact filename varies by
        // crate-type because rustc's default naming convention does.
        let dir = layout.host_artifact_dir(crate_def);
        let path = match crate_def.crate_type {
            CrateType::Bin => dir.join(format!("{crate_name_rustc}{extra_filename}")),
            CrateType::ProcMacro => {
                let ext = std::env::consts::DLL_EXTENSION;
                dir.join(format!(
                    "{}{}{}{}",
                    std::env::consts::DLL_PREFIX,
                    crate_name_rustc,
                    extra_filename,
                    if ext.is_empty() {
                        String::new()
                    } else {
                        format!(".{ext}")
                    }
                ))
            }
            _ => dir.join(format!("lib{crate_name_rustc}{extra_filename}.rlib")),
        };
        return Ok(path);
    }

    // Cross crates: dispatch on crate-type for output location and naming.
    // Mirrors command_builder.rs:114-183.
    let target = model.targets.get(cref.target).ok_or_else(|| {
        Error::Compile(format!(
            "expected_crate_artifact_path: crate '{}' references target handle \
             {:?} which is not in the build model",
            crate_def.name, cref.target,
        ))
    })?;

    let path = match crate_def.crate_type {
        CrateType::Bin => {
            // Cross binaries land directly in `cross_final_dir`. No
            // extra-filename suffix; the canonical name is just the
            // crate name with a target-specific exe suffix appended.
            let final_dir = layout.cross_final_dir(target, &resolved.profile);
            let suffix = exe_suffix_for_target(&target.spec);
            final_dir.join(format!("{crate_name_rustc}{suffix}"))
        }
        CrateType::RawBinary => {
            // `--oformat=binary` flat output renamed to `.bin`.
            let final_dir = layout.cross_final_dir(target, &resolved.profile);
            final_dir.join(format!("{crate_name_rustc}.bin"))
        }
        _ => {
            // Libs and proc-macros: per-crate dir under the cross tree
            // with the standard `lib<name>-gluon-<name>.rlib` shape.
            let dir = layout.cross_artifact_dir(target, &resolved.profile, crate_def);
            dir.join(format!("lib{crate_name_rustc}{extra_filename}.rlib"))
        }
    };
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{
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
                panic_strategy: None,
                span: None,
            },
        );
        h
    }

    fn make_resolved(
        target: Handle<TargetDef>,
        crates: Vec<ResolvedCrateRef>,
    ) -> ResolvedConfig {
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
                target,
                opt_level: 0,
                debug_info: true,
                lto: None,
                boot_binary: None,
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
    fn cross_lib_artifact_lands_under_cross_artifact_dir() {
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
        assert!(s.contains("/cross/x86_64-unknown-none/dev/kernel/"), "{s}");
        assert!(s.ends_with("/libkernel-gluon-kernel.rlib"), "{s}");
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
        let th = make_target(
            &mut model,
            "host-target",
            "x86_64-unknown-linux-gnu",
        );
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
        assert!(s.contains("/host/tooling/"), "{s}");
        assert!(s.ends_with("/libtooling-gluon-tooling.rlib"), "{s}");
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
}
