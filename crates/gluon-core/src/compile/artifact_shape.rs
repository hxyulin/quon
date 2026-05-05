//! `CrateArtifactShape` — single source of truth for "given a
//! `(layout, model, resolved, crate_def, cref)`, where does its
//! rustc output land and where does the depfile land?"
//!
//! Both [`super::command_builder::build_rustc_command`] (write
//! side — assembles the rustc invocation that produces the output)
//! and [`super::artifact_path::expected_crate_artifact_path`] (read
//! side — looks up the path after the fact for analyzer / scheduler
//! bookkeeping) consume this type. There is exactly one
//! implementation by design — see `docs/superpowers/plans/2026-05-03-stage-6-compile-pipeline.md`
//! "Decisions baked in" §1 for why this is a struct and not a
//! trait.

use crate::compile::compile_utils::{exe_suffix_for_target, normalize_crate_name};
use crate::compile::layout::BuildLayout;
use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateDef, CrateType, ResolvedConfig, ResolvedCrateRef};
use std::path::PathBuf;

/// Layout-derived artifact paths for a single crate compilation.
///
/// All fields are deterministic: the same `(layout, model,
/// resolved, crate_def, cref)` always yields the same
/// `CrateArtifactShape`. No filesystem state is consulted.
///
/// `output_path` is the *real* on-disk artifact (rlib / dylib /
/// bin / `.bin`). Callers that need a different freshness stamp
/// for non-codegen drivers (e.g. `gluon check` swaps to the depfile)
/// apply that swap themselves — this type intentionally does not
/// know about driver kinds.
#[derive(Debug, Clone)]
pub struct CrateArtifactShape {
    /// Directory rustc emits into (`--out-dir` for libs / proc-macros;
    /// `cross_final_dir` for executables).
    pub out_dir: PathBuf,
    /// On-disk path of the primary artifact (rlib / dylib / bin /
    /// `.bin`).
    pub output_path: PathBuf,
    /// On-disk path of the rustc-emitted depfile (`.d`).
    pub depfile_path: PathBuf,
    /// On-disk path of an auxiliary debug ELF, when one exists. Present
    /// only for `CrateType::RawBinary`, where rustc emits `<name>.elf`
    /// and a post-link step transforms it into the flat `<name>.bin`
    /// the build model treats as the primary artifact. Every other
    /// crate type leaves this `None`.
    pub debug_elf_path: Option<PathBuf>,
}

impl CrateArtifactShape {
    /// Compute the artifact shape for `crate_def` under `layout` /
    /// `resolved`. `cref.host` selects host-vs-cross dispatch;
    /// `crate_def.crate_type` selects the filename shape.
    ///
    /// Errors only on a cross crate whose target handle is not in
    /// the build model (a model-integrity bug).
    pub fn for_crate(
        layout: &BuildLayout,
        model: &BuildModel,
        resolved: &ResolvedConfig,
        crate_def: &CrateDef,
        cref: &ResolvedCrateRef,
    ) -> Result<Self> {
        let crate_name_rustc = normalize_crate_name(&crate_def.name);
        let extra_filename = format!("-gluon-{crate_name_rustc}");

        if cref.host {
            let out_dir = layout.host_artifact_dir(crate_def);
            let output_path = match crate_def.crate_type {
                CrateType::Bin => out_dir.join(format!("{crate_name_rustc}{extra_filename}")),
                CrateType::ProcMacro => {
                    let ext = std::env::consts::DLL_EXTENSION;
                    out_dir.join(format!(
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
                _ => out_dir.join(format!("lib{crate_name_rustc}{extra_filename}.rlib")),
            };
            let depfile_path = out_dir.join(format!("{crate_name_rustc}{extra_filename}.d"));
            return Ok(Self {
                out_dir,
                output_path,
                depfile_path,
                debug_elf_path: None,
            });
        }

        let target = model.targets.get(cref.target).ok_or_else(|| {
            Error::Compile(format!(
                "crate '{}': target handle {:?} not found in build model",
                crate_def.name, cref.target
            ))
        })?;

        match crate_def.crate_type {
            CrateType::Bin => {
                // Cross executables land directly in `cross_final_dir`
                // with no `extra_filename` (rustc never hash-suffixes
                // bin names; adding extra-filename produces an ugly
                // `name-gluon-name` final binary). Depfile follows the
                // same no-extra rule.
                let out_dir = layout.cross_final_dir(target, &resolved.profile);
                let suffix = exe_suffix_for_target(&target.spec);
                let output_path = out_dir.join(format!("{crate_name_rustc}{suffix}"));
                let depfile_path = out_dir.join(format!("{crate_name_rustc}.d"));
                Ok(Self {
                    out_dir,
                    output_path,
                    depfile_path,
                    debug_elf_path: None,
                })
            }
            CrateType::RawBinary => {
                // rustc emits `<name>.elf` into cross_final_dir; a
                // post-link objcopy step converts it to the flat
                // `<name>.bin`. Both paths are recorded here so the
                // freshness check and callers can find both.
                let out_dir = layout.cross_final_dir(target, &resolved.profile);
                let output_path = out_dir.join(format!("{crate_name_rustc}.bin"));
                let depfile_path = out_dir.join(format!("{crate_name_rustc}.d"));
                let debug_elf_path = Some(out_dir.join(format!("{crate_name_rustc}.elf")));
                Ok(Self {
                    out_dir,
                    output_path,
                    depfile_path,
                    debug_elf_path,
                })
            }
            _ => {
                // Libs and proc-macros: per-crate dir under the cross
                // tree with the standard `lib<name>-gluon-<name>.rlib`
                // shape.
                let out_dir = layout.cross_artifact_dir(target, &resolved.profile, crate_def);
                let output_path =
                    out_dir.join(format!("lib{crate_name_rustc}{extra_filename}.rlib"));
                let depfile_path = out_dir.join(format!("{crate_name_rustc}{extra_filename}.d"));
                Ok(Self {
                    out_dir,
                    output_path,
                    depfile_path,
                    debug_elf_path: None,
                })
            }
        }
    }
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

    // Fixture helpers mirror those in `compile/artifact_path.rs::tests`.
    // They are duplicated rather than extracted to a shared test
    // utility module — the existing pattern in this crate is to
    // inline test fixtures, and YAGNI says don't introduce a shared
    // module for two callers.

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
    fn host_lib_emits_rlib_into_host_artifact_dir() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "host", "x86_64-unknown-linux-gnu");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "core_utils".into(),
            CrateDef {
                name: "core_utils".into(),
                path: "core_utils".into(),
                crate_type: CrateType::Lib,
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
        let crate_def = model.crates.get(ch).unwrap();
        let cref = &resolved.crates[0];
        let shape =
            CrateArtifactShape::for_crate(&layout(), &model, &resolved, crate_def, cref).unwrap();

        assert_eq!(shape.out_dir, layout().host_artifact_dir(crate_def));
        let s = shape.output_path.to_string_lossy().replace('\\', "/");
        assert!(s.contains("/host/core_utils/"), "{s}");
        assert!(s.ends_with("/libcore_utils-gluon-core_utils.rlib"), "{s}");
        let d = shape.depfile_path.to_string_lossy().replace('\\', "/");
        assert!(d.ends_with("/core_utils-gluon-core_utils.d"), "{d}");
    }

    #[test]
    fn host_proc_macro_uses_dll_prefix_and_extension() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "host", "x86_64-unknown-linux-gnu");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "macros".into(),
            CrateDef {
                name: "macros".into(),
                path: "macros".into(),
                crate_type: CrateType::ProcMacro,
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
        let crate_def = model.crates.get(ch).unwrap();
        let cref = &resolved.crates[0];
        let shape =
            CrateArtifactShape::for_crate(&layout(), &model, &resolved, crate_def, cref).unwrap();

        let s = shape.output_path.to_string_lossy().replace('\\', "/");
        let prefix = std::env::consts::DLL_PREFIX;
        let ext = std::env::consts::DLL_EXTENSION;
        let expected_filename = if ext.is_empty() {
            format!("{prefix}macros-gluon-macros")
        } else {
            format!("{prefix}macros-gluon-macros.{ext}")
        };
        assert!(
            s.ends_with(&expected_filename),
            "{s} vs {expected_filename}"
        );
        assert!(s.contains("/host/macros/"), "{s}");
    }

    #[test]
    fn host_bin_has_no_extension() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "host", "x86_64-unknown-linux-gnu");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "tooling".into(),
            CrateDef {
                name: "tooling".into(),
                path: "tooling".into(),
                crate_type: CrateType::Bin,
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
        let crate_def = model.crates.get(ch).unwrap();
        let cref = &resolved.crates[0];
        let shape =
            CrateArtifactShape::for_crate(&layout(), &model, &resolved, crate_def, cref).unwrap();
        // Host bins keep extra_filename (mirrors command_builder's
        // host-Bin arm: `out_dir/{name}{extra}`).
        let s = shape.output_path.to_string_lossy().replace('\\', "/");
        assert!(s.ends_with("/tooling-gluon-tooling"), "{s}");
        assert!(s.contains("/host/tooling/"), "{s}");
    }

    #[test]
    fn cross_lib_emits_rlib_into_cross_artifact_dir() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-unknown-none", "x86_64-unknown-none");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
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
        let crate_def = model.crates.get(ch).unwrap();
        let cref = &resolved.crates[0];
        let shape =
            CrateArtifactShape::for_crate(&layout(), &model, &resolved, crate_def, cref).unwrap();
        let s = shape.output_path.to_string_lossy().replace('\\', "/");
        assert!(s.contains("/cross/x86_64-unknown-none/dev/kernel/"), "{s}");
        assert!(s.ends_with("/libkernel-gluon-kernel.rlib"), "{s}");
        let d = shape.depfile_path.to_string_lossy().replace('\\', "/");
        assert!(d.ends_with("/kernel-gluon-kernel.d"), "{d}");
    }

    #[test]
    fn cross_bin_lands_in_cross_final_dir_no_extra_filename() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-unknown-none", "x86_64-unknown-none");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                crate_type: CrateType::Bin,
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
        let crate_def = model.crates.get(ch).unwrap();
        let cref = &resolved.crates[0];
        let shape =
            CrateArtifactShape::for_crate(&layout(), &model, &resolved, crate_def, cref).unwrap();
        let s = shape.output_path.to_string_lossy().replace('\\', "/");
        // Bare-metal target: empty exe suffix. Output is just the
        // crate name in cross_final_dir.
        assert!(s.contains("/cross/x86_64-unknown-none/dev/final/"), "{s}");
        assert!(s.ends_with("/kernel"), "{s}");
        // Depfile has no extra-filename for cross executables.
        let d = shape.depfile_path.to_string_lossy().replace('\\', "/");
        assert!(d.ends_with("/kernel.d"), "{d}");
        assert!(
            !d.contains("-gluon-"),
            "depfile should have no extra-filename: {d}"
        );
    }

    #[test]
    fn cross_raw_binary_lands_in_cross_final_dir_with_bin_suffix() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "i386-code16", "i386-unknown-none-code16");
        let g = ensure_group(&mut model);
        let (ch, _) = model.crates.insert(
            "boot-sector".into(),
            CrateDef {
                name: "boot-sector".into(),
                path: "boot-sector".into(),
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
        let crate_def = model.crates.get(ch).unwrap();
        let cref = &resolved.crates[0];
        let shape =
            CrateArtifactShape::for_crate(&layout(), &model, &resolved, crate_def, cref).unwrap();
        let s = shape.output_path.to_string_lossy().replace('\\', "/");
        assert!(s.contains("/cross/i386-code16/dev/final/"), "{s}");
        // Hyphenated names normalise to underscores for rustc.
        assert!(s.ends_with("/boot_sector.bin"), "{s}");
        assert_eq!(
            shape.debug_elf_path.as_deref(),
            Some(shape.out_dir.join("boot_sector.elf").as_path()),
            "RawBinary must declare a sibling .elf path next to the .bin"
        );
    }
}
