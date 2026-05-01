//! Merge [`QemuDef`] + [`ResolvedProfile`] into a fully-defaulted
//! [`ResolvedQemu`].
//!
//! The `gluon.rhai` schema lets users set `qemu(...)` knobs at the
//! project level *and* override a subset of them per profile
//! (`profile.qemu_memory(...)`, `.qemu_cores(...)`, `.qemu_extra_args(...)`,
//! `.test_timeout(...)`). Resolving merges these two sources into a
//! single flat record with every field populated.
//!
//! Precedence, most specific first:
//!
//! 1. Profile-level override (`ResolvedProfile::qemu_memory` etc.)
//! 2. Project-level `QemuDef` field
//! 3. A hard-coded gluon default
//!
//! Profile-level `qemu_extra_args` are *appended* to the project-level
//! `extra_args`, not replacing them. That matches how `extra_args`
//! works everywhere else in gluon (additive rather than override).

use crate::error::{Error, Result};
use gluon_model::{
    BootMode, BuildModel, BuildStepDef, DiskSource, DisplayMode, EspSource, Handle, ImageDef,
    QemuDef, ResolvedProfile, SerialMode,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Fully-resolved QEMU configuration ready for [`super::build_qemu_command`].
#[derive(Debug, Clone)]
pub struct ResolvedQemu {
    pub binary: String,
    pub machine: String,
    pub memory_mb: u32,
    pub cores: u32,
    pub serial: SerialMode,
    /// Display policy. `None` means "user said nothing" — the runner
    /// keeps its implicit `-display none` default to stay headless for
    /// CI / test flows. A `Some(...)` value reflects an explicit
    /// `qemu().display(...)` call from the user's gluon.rhai.
    pub display: Option<DisplayMode>,
    pub extra_args: Vec<String>,
    pub boot_mode: BootMode,
    pub ovmf_code: Option<PathBuf>,
    pub ovmf_vars: Option<PathBuf>,
    pub esp: Option<EspSource>,
    /// Resolved boot disk path. `None` means no disk wired in (the
    /// runner falls back to `esp` or direct-kernel boot). `Some(path)`
    /// is the canonical raw image to attach as the first IDE drive in
    /// BIOS mode, or as a generic raw drive in UEFI mode.
    ///
    /// `DiskSource::ImageRef("name")` is resolved to a path here via
    /// the build summary's `image_paths` map. `DiskSource::Image(path)`
    /// is passed through untouched. The lookup happens in `run::entry`
    /// after the scheduler has produced the image, so this field is
    /// always either `None` or a concrete path.
    pub disk: Option<PathBuf>,
    pub test_exit_port: u16,
    pub test_success_code: u8,
    pub timeout: Option<Duration>,
}

impl ResolvedQemu {
    /// Default machine type. `q35` works for both direct-kernel and UEFI
    /// boot and is the modern chipset every OVMF build targets.
    pub const DEFAULT_MACHINE: &'static str = "q35";
    pub const DEFAULT_MEMORY_MB: u32 = 256;
    pub const DEFAULT_CORES: u32 = 1;
    pub const DEFAULT_TEST_EXIT_PORT: u16 = 0xf4;
    pub const DEFAULT_TEST_SUCCESS_CODE: u8 = 0x10;
}

/// Pick a default QEMU system binary for a target triple when the user
/// didn't set one explicitly via `qemu("...")`.
///
/// We match on the architecture prefix of the triple (everything up to
/// the first `-`). This covers the common bare-metal cases without
/// baking in a full triple → binary table that'd rot every time rustc
/// adds a target. Anything we don't recognise becomes
/// [`Error::UnknownQemuTarget`] — we'd rather fail loudly than silently
/// default to `qemu-system-x86_64` and boot the wrong architecture.
///
/// The triple we receive is `TargetDef::spec` for builtin targets
/// (a rustc-known triple like `x86_64-unknown-none`) and
/// `TargetDef::name` as a fallback for custom JSON specs. For custom
/// specs users typically name the file after the arch anyway
/// (`my-kernel-x86_64.json` → `x86_64`), so the prefix match still
/// tends to work — and if it doesn't, the error tells them exactly
/// what to do.
pub fn default_binary_for_target(triple: &str) -> Result<String> {
    // Architecture prefix, i.e. everything before the first `-`. Empty
    // triples and triples without a dash get treated as the whole
    // string.
    let arch = triple.split('-').next().unwrap_or(triple);
    let suffix = match arch {
        "x86_64" => "x86_64",
        "i686" | "i586" | "i386" => "i386",
        "aarch64" | "arm64" => "aarch64",
        "arm" | "armv7" | "armv7a" | "armv7r" | "thumbv7em" | "thumbv7m" | "thumbv6m"
        | "thumbv8m" => "arm",
        "riscv64" | "riscv64gc" | "riscv64imac" => "riscv64",
        "riscv32" | "riscv32i" | "riscv32imc" | "riscv32imac" => "riscv32",
        _ => {
            return Err(Error::UnknownQemuTarget {
                triple: triple.to_string(),
            });
        }
    };
    Ok(format!("qemu-system-{suffix}"))
}

/// Merge `qemu` + `profile` into a [`ResolvedQemu`] with defaults applied.
///
/// `boot_mode_override` lets the CLI force `--uefi` / `--direct` on top
/// of whatever the profile declared. `target_triple` is used only as
/// input to [`default_binary_for_target`] when the user didn't set
/// `qemu("...")` explicitly.
pub fn resolve_qemu(
    qemu: &QemuDef,
    profile: &ResolvedProfile,
    target_triple: &str,
    boot_mode_override: Option<BootMode>,
    timeout_override: Option<Duration>,
) -> Result<ResolvedQemu> {
    // Start with project-level extra_args, then append profile-level.
    // The order matters for flags like `-display none` that the user
    // may want to override at the profile level.
    let mut extra_args = qemu.extra_args.clone();
    extra_args.extend(profile.qemu_extra_args.iter().cloned());

    let timeout =
        timeout_override.or_else(|| profile.test_timeout.map(|s| Duration::from_secs(s as u64)));

    let binary = match qemu.binary.clone() {
        Some(b) => b,
        // Only compute the per-target default when the user omitted
        // `qemu("...")`. This keeps the error path scoped — a user who
        // *did* set the binary explicitly shouldn't be blocked by an
        // unrecognised target arch.
        None => default_binary_for_target(target_triple)?,
    };

    Ok(ResolvedQemu {
        binary,
        machine: qemu
            .machine
            .clone()
            .unwrap_or_else(|| ResolvedQemu::DEFAULT_MACHINE.to_string()),
        memory_mb: profile
            .qemu_memory
            .or(qemu.memory_mb)
            .unwrap_or(ResolvedQemu::DEFAULT_MEMORY_MB),
        cores: profile
            .qemu_cores
            .or(qemu.cores)
            .unwrap_or(ResolvedQemu::DEFAULT_CORES),
        serial: qemu.serial.clone().unwrap_or(SerialMode::Stdio),
        display: qemu.display.clone(),
        extra_args,
        boot_mode: boot_mode_override
            .or(profile.qemu_boot_mode)
            .or(qemu.boot_mode)
            .unwrap_or(BootMode::Direct),
        ovmf_code: qemu.ovmf_code.clone(),
        ovmf_vars: qemu.ovmf_vars.clone(),
        esp: qemu.esp.clone(),
        // Disk is resolved separately via `resolve_disk_source` once
        // the caller has the scheduler's `image_paths` in hand. We
        // initialise to `None` here; `run::entry` fills it in.
        disk: None,
        test_exit_port: qemu
            .test_exit_port
            .unwrap_or(ResolvedQemu::DEFAULT_TEST_EXIT_PORT),
        test_success_code: qemu
            .test_success_code
            .unwrap_or(ResolvedQemu::DEFAULT_TEST_SUCCESS_CODE),
        timeout,
    })
}

/// Resolve a [`DiskSource`] to a concrete [`PathBuf`].
///
/// Two cases:
///
/// - [`DiskSource::Image`] is a fixed path supplied by the user. We
///   return it untouched. The runner checks the path exists at spawn
///   time (via the existing `skip_path_checks` knob).
/// - [`DiskSource::ImageRef(name)`] points at an `image("name")`
///   declared in the same script. We look up the [`ImageDef`] handle
///   in `model.images`, then look up that handle in `image_paths` (the
///   scheduler's per-build output map). If the user named an image
///   that doesn't exist, we error with the available names; if the
///   image exists but the build summary doesn't have it, that's an
///   ordering bug we surface loudly.
///
/// `image_paths` may be empty in dry-run / `--no-build` flows. In
/// those modes we fall back to path arithmetic via
/// `BuildLayout::image_path` (caller's responsibility — pass the
/// fallback as `image_paths` populated from the layout).
pub fn resolve_disk_source(
    disk: &DiskSource,
    model: &BuildModel,
    image_paths: &BTreeMap<Handle<ImageDef>, PathBuf>,
    build_step_paths: &BTreeMap<Handle<BuildStepDef>, PathBuf>,
) -> Result<PathBuf> {
    match disk {
        DiskSource::Image { path } => Ok(path.clone()),
        DiskSource::ImageRef { name } => {
            // The unified artifact namespace lets `disk_ref(name)`
            // resolve to either an `image()` block or a `build_step()`
            // output. Validate has rejected name collisions, so at
            // most one resolves. We try image first (the historical
            // case) and fall back to build step.
            if let Some(handle) = model.images.lookup(name) {
                return image_paths.get(&handle).cloned().ok_or_else(|| {
                    Error::Compile(format!(
                        "qemu.disk_ref(\"{name}\"): image is declared but the scheduler \
                         produced no output path for it. (This typically means a build \
                         ordering bug — the image node should run before qemu launch.)"
                    ))
                });
            }
            if let Some(handle) = model.build_steps.lookup(name) {
                return build_step_paths.get(&handle).cloned().ok_or_else(|| {
                    Error::Compile(format!(
                        "qemu.disk_ref(\"{name}\"): build step is declared but the scheduler \
                         produced no output path for it. (This typically means a build \
                         ordering bug — the BuildStep node should run before qemu launch.)"
                    ))
                });
            }
            // Neither namespace resolved.
            let mut available: Vec<&str> = model
                .images
                .iter()
                .map(|(_, i): (_, &ImageDef)| i.name.as_str())
                .collect();
            for (_, bs) in model.build_steps.iter() {
                available.push(bs.name.as_str());
            }
            available.sort();
            Err(Error::Compile(format!(
                "qemu.disk_ref(\"{name}\") references unknown artifact (no image or \
                 build step with that name); available: [{}]",
                available.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::ImageDef;
    use gluon_model::handle::Handle;

    fn empty_profile() -> ResolvedProfile {
        ResolvedProfile {
            name: "dev".into(),
            target: Handle::new(0),
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

    const X86_TRIPLE: &str = "x86_64-unknown-none";

    #[test]
    fn defaults_everywhere_when_unset() {
        let r = resolve_qemu(
            &QemuDef::default(),
            &empty_profile(),
            X86_TRIPLE,
            None,
            None,
        )
        .expect("resolve");
        assert_eq!(r.binary, "qemu-system-x86_64");
        assert_eq!(r.machine, ResolvedQemu::DEFAULT_MACHINE);
        assert_eq!(r.memory_mb, ResolvedQemu::DEFAULT_MEMORY_MB);
        assert_eq!(r.cores, ResolvedQemu::DEFAULT_CORES);
        assert_eq!(r.serial, SerialMode::Stdio);
        assert!(r.extra_args.is_empty());
        assert_eq!(r.boot_mode, BootMode::Direct);
        assert!(r.ovmf_code.is_none());
        assert!(r.ovmf_vars.is_none());
        assert!(r.esp.is_none());
        assert_eq!(r.test_exit_port, 0xf4);
        assert_eq!(r.test_success_code, 0x10);
        assert!(r.timeout.is_none());
    }

    #[test]
    fn profile_override_beats_qemu_def() {
        let qemu = QemuDef {
            memory_mb: Some(128),
            cores: Some(1),
            ..Default::default()
        };
        let mut prof = empty_profile();
        prof.qemu_memory = Some(512);
        prof.qemu_cores = Some(4);

        let r = resolve_qemu(&qemu, &prof, X86_TRIPLE, None, None).expect("resolve");
        assert_eq!(r.memory_mb, 512);
        assert_eq!(r.cores, 4);
    }

    #[test]
    fn extra_args_append_in_order() {
        let qemu = QemuDef {
            extra_args: vec!["-display".into(), "none".into()],
            ..Default::default()
        };
        let mut prof = empty_profile();
        prof.qemu_extra_args = vec!["-d".into(), "int".into()];

        let r = resolve_qemu(&qemu, &prof, X86_TRIPLE, None, None).expect("resolve");
        assert_eq!(r.extra_args, vec!["-display", "none", "-d", "int"]);
    }

    #[test]
    fn profile_qemu_boot_mode_wins_over_qemu_def() {
        let qemu = QemuDef {
            boot_mode: Some(BootMode::Direct),
            ..Default::default()
        };
        let mut prof = empty_profile();
        prof.qemu_boot_mode = Some(BootMode::Bios);
        let r = resolve_qemu(&qemu, &prof, X86_TRIPLE, None, None).unwrap();
        assert_eq!(r.boot_mode, BootMode::Bios);
    }

    #[test]
    fn cli_boot_mode_override_beats_profile_qemu_boot_mode() {
        let mut prof = empty_profile();
        prof.qemu_boot_mode = Some(BootMode::Bios);
        let r = resolve_qemu(
            &QemuDef::default(),
            &prof,
            X86_TRIPLE,
            Some(BootMode::Uefi),
            None,
        )
        .unwrap();
        assert_eq!(r.boot_mode, BootMode::Uefi);
    }

    #[test]
    fn boot_mode_override_wins() {
        let qemu = QemuDef {
            boot_mode: Some(BootMode::Direct),
            ..Default::default()
        };
        let r = resolve_qemu(
            &qemu,
            &empty_profile(),
            X86_TRIPLE,
            Some(BootMode::Uefi),
            None,
        )
        .expect("resolve");
        assert_eq!(r.boot_mode, BootMode::Uefi);
    }

    #[test]
    fn timeout_cli_override_beats_profile() {
        let mut prof = empty_profile();
        prof.test_timeout = Some(30);
        let r = resolve_qemu(
            &QemuDef::default(),
            &prof,
            X86_TRIPLE,
            None,
            Some(Duration::from_secs(60)),
        )
        .expect("resolve");
        assert_eq!(r.timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn timeout_profile_fallback() {
        let mut prof = empty_profile();
        prof.test_timeout = Some(30);
        let r = resolve_qemu(&QemuDef::default(), &prof, X86_TRIPLE, None, None).expect("resolve");
        assert_eq!(r.timeout, Some(Duration::from_secs(30)));
    }

    // ---- default_binary_for_target ----------------------------------------

    #[test]
    fn default_binary_for_common_triples() {
        assert_eq!(
            default_binary_for_target("x86_64-unknown-none").unwrap(),
            "qemu-system-x86_64"
        );
        assert_eq!(
            default_binary_for_target("aarch64-unknown-none").unwrap(),
            "qemu-system-aarch64"
        );
        assert_eq!(
            default_binary_for_target("riscv64gc-unknown-none-elf").unwrap(),
            "qemu-system-riscv64"
        );
        assert_eq!(
            default_binary_for_target("riscv32imac-unknown-none-elf").unwrap(),
            "qemu-system-riscv32"
        );
        assert_eq!(
            default_binary_for_target("thumbv7em-none-eabihf").unwrap(),
            "qemu-system-arm"
        );
        assert_eq!(
            default_binary_for_target("i686-unknown-linux-gnu").unwrap(),
            "qemu-system-i386"
        );
    }

    #[test]
    fn default_binary_errors_on_unknown_arch() {
        let err = default_binary_for_target("sparc64-unknown-none").unwrap_err();
        assert!(
            matches!(err, Error::UnknownQemuTarget { ref triple } if triple == "sparc64-unknown-none")
        );
        // The error message must tell the user what to do.
        let msg = err.to_string();
        assert!(
            msg.contains("qemu(\"qemu-system-"),
            "diagnostic should point at the fix: {msg}"
        );
    }

    #[test]
    fn explicit_binary_bypasses_target_probe_even_for_unknown_arch() {
        let qemu = QemuDef {
            binary: Some("qemu-system-xtensa".into()),
            ..Default::default()
        };
        // sparc64 is unknown to default_binary_for_target, but since
        // the user set binary explicitly we should not call it.
        let r = resolve_qemu(&qemu, &empty_profile(), "sparc64-unknown-none", None, None)
            .expect("explicit binary should bypass target probe");
        assert_eq!(r.binary, "qemu-system-xtensa");
    }

    #[test]
    fn default_binary_derives_from_target_when_unset() {
        let r = resolve_qemu(
            &QemuDef::default(),
            &empty_profile(),
            "aarch64-unknown-none",
            None,
            None,
        )
        .expect("resolve");
        assert_eq!(r.binary, "qemu-system-aarch64");
    }

    // -----------------------------------------------------------------
    // resolve_disk_source
    // -----------------------------------------------------------------

    #[test]
    fn resolve_disk_image_returns_path_unchanged() {
        let model = BuildModel::default();
        let map: BTreeMap<Handle<ImageDef>, PathBuf> = BTreeMap::new();
        let bs_map: BTreeMap<Handle<BuildStepDef>, PathBuf> = BTreeMap::new();
        let disk = DiskSource::Image {
            path: PathBuf::from("/tmp/my.img"),
        };
        let p = resolve_disk_source(&disk, &model, &map, &bs_map).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/my.img"));
    }

    #[test]
    fn resolve_disk_ref_looks_up_image_path() {
        let mut model = BuildModel::default();
        let (h, _) = model.images.insert(
            "boot-disk".into(),
            ImageDef {
                name: "boot-disk".into(),
                ..Default::default()
            },
        );
        let mut map = BTreeMap::new();
        map.insert(h, PathBuf::from("/build/images/boot-disk.img"));
        let bs_map: BTreeMap<Handle<BuildStepDef>, PathBuf> = BTreeMap::new();

        let disk = DiskSource::ImageRef {
            name: "boot-disk".into(),
        };
        let p = resolve_disk_source(&disk, &model, &map, &bs_map).unwrap();
        assert_eq!(p, PathBuf::from("/build/images/boot-disk.img"));
    }

    #[test]
    fn resolve_disk_ref_looks_up_build_step_path() {
        // disk_ref to a build step's name resolves through the
        // build_step_paths map. Uses the unified artifact namespace —
        // user can boot directly from a build-step output without an
        // intermediate image() wrapper.
        let mut model = BuildModel::default();
        let (h, _) = model.build_steps.insert(
            "disk-image".into(),
            BuildStepDef {
                name: "disk-image".into(),
                runner: "image-builder".into(),
                output: "disk.img".into(),
                ..Default::default()
            },
        );
        let map: BTreeMap<Handle<ImageDef>, PathBuf> = BTreeMap::new();
        let mut bs_map: BTreeMap<Handle<BuildStepDef>, PathBuf> = BTreeMap::new();
        bs_map.insert(h, PathBuf::from("/build/build-steps/disk-image/disk.img"));

        let disk = DiskSource::ImageRef {
            name: "disk-image".into(),
        };
        let p = resolve_disk_source(&disk, &model, &map, &bs_map).unwrap();
        assert_eq!(
            p,
            PathBuf::from("/build/build-steps/disk-image/disk.img")
        );
    }

    #[test]
    fn resolve_disk_ref_unknown_image_lists_available() {
        let mut model = BuildModel::default();
        model.images.insert(
            "alpha".into(),
            ImageDef {
                name: "alpha".into(),
                ..Default::default()
            },
        );
        model.images.insert(
            "beta".into(),
            ImageDef {
                name: "beta".into(),
                ..Default::default()
            },
        );
        let map = BTreeMap::new();
        let bs_map: BTreeMap<Handle<BuildStepDef>, PathBuf> = BTreeMap::new();

        let disk = DiskSource::ImageRef {
            name: "gamma".into(),
        };
        let err = resolve_disk_source(&disk, &model, &map, &bs_map).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gamma"));
        assert!(msg.contains("alpha") && msg.contains("beta"));
    }
}
