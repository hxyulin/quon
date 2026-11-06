//! Merge the runner's [`QemuDef`] settings and their [`Overrides`]
//! into a fully-defaulted [`ResolvedQemu`].
//!
//! The `gluon.rs` schema lets users set `qemu(...)` knobs at the
//! project level *and* override a subset of them per profile
//! (`profile.qemu_memory(...)`, `.qemu_cores(...)`, `.qemu_extra_args(...)`,
//! `.qemu_boot_mode(...)`). Gluon passes the profile's values, and a
//! boot mode flag from the command line, as the request's overrides.
//! Resolving merges the two into a single flat record with every field
//! populated.
//!
//! Precedence, most specific first:
//!
//! 1. An override (command line, then profile; Gluon merges those)
//! 2. The runner-level `QemuDef` field
//! 3. A hard-coded default
//!
//! Override `extra_args` are *appended* to the runner-level
//! `extra_args`, not replacing them. That matches how `extra_args`
//! works everywhere else in gluon (additive rather than override).

use crate::error::{Error, Result};
use gluon_model::{BootMode, DiskSource, DisplayMode, EspSource, QemuDef, SerialMode};
use gluon_runner::{Artifact, ArtifactKind};
use serde::Deserialize;
use std::path::PathBuf;

/// The settings a profile or the command line may override. Unknown
/// keys are refused, so a misspelt override fails instead of being
/// ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    #[serde(default)]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub cores: Option<u32>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub boot_mode: Option<BootMode>,
}

/// Fully-resolved QEMU configuration ready for [`crate::build_qemu_command`].
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
    /// `qemu().display(...)` call from the user's gluon.rs.
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
    /// `DiskSource::ImageRef("name")` is resolved to a path through
    /// the request's artifacts. `DiskSource::Image(path)` is passed
    /// through untouched. The lookup happens in `prepare`, after the
    /// build, so this field is always either `None` or a concrete path.
    pub disk: Option<PathBuf>,
    pub test_exit_port: u16,
    pub test_success_code: u8,
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

/// Merge `qemu` + `overrides` into a [`ResolvedQemu`] with defaults
/// applied. `target_triple` is used only as input to
/// [`default_binary_for_target`] when the user didn't set
/// `qemu("...")` explicitly.
pub fn resolve_qemu(
    qemu: &QemuDef,
    overrides: &Overrides,
    target_triple: &str,
) -> Result<ResolvedQemu> {
    // Start with runner-level extra_args, then append the overrides'.
    // The order matters for flags like `-display none` that the user
    // may want to override at the profile level.
    let mut extra_args = qemu.extra_args.clone();
    extra_args.extend(overrides.extra_args.iter().cloned());

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
        memory_mb: overrides
            .memory_mb
            .or(qemu.memory_mb)
            .unwrap_or(ResolvedQemu::DEFAULT_MEMORY_MB),
        cores: overrides
            .cores
            .or(qemu.cores)
            .unwrap_or(ResolvedQemu::DEFAULT_CORES),
        serial: qemu.serial.clone().unwrap_or(SerialMode::Stdio),
        display: qemu.display.clone(),
        extra_args,
        boot_mode: overrides
            .boot_mode
            .or(qemu.boot_mode)
            .unwrap_or(BootMode::Direct),
        ovmf_code: qemu.ovmf_code.clone(),
        ovmf_vars: qemu.ovmf_vars.clone(),
        esp: qemu.esp.clone(),
        // Disk is resolved separately via `resolve_disk_source` once
        // the caller has the artifacts in hand.
        disk: None,
        test_exit_port: qemu
            .test_exit_port
            .unwrap_or(ResolvedQemu::DEFAULT_TEST_EXIT_PORT),
        test_success_code: qemu
            .test_success_code
            .unwrap_or(ResolvedQemu::DEFAULT_TEST_SUCCESS_CODE),
    })
}

/// Resolve a [`DiskSource`] to a concrete [`PathBuf`].
///
/// Two cases:
///
/// - [`DiskSource::Image`] is a fixed path supplied by the user. We
///   return it untouched. The boot-mode assembler checks the path
///   exists when the build ran.
/// - [`DiskSource::ImageRef(name)`] points at an `image("name")` or a
///   `build_step("name")` declared in the same script, and resolves
///   through the request's artifacts. If the user named an artifact
///   that doesn't exist, we error with the available names; if it
///   exists but the build produced no path for it, that's an ordering
///   bug we surface loudly.
///
/// Under `--dry-run` and `--no-build` Gluon passes the paths the build
/// would use, so a reference still resolves.
pub fn resolve_disk_source(disk: &DiskSource, artifacts: &[Artifact]) -> Result<PathBuf> {
    match disk {
        DiskSource::Image { path } => Ok(path.clone()),
        DiskSource::ImageRef { name } => {
            // The unified artifact namespace lets `disk_ref(name)`
            // resolve to either an `image()` block or a `build_step()`
            // output. Validate has rejected name collisions, so at
            // most one resolves. We try image first (the historical
            // case) and fall back to build step.
            let find = |kind| {
                artifacts
                    .iter()
                    .find(|a: &&Artifact| a.kind == kind && a.name == *name)
            };
            if let Some(a) = find(ArtifactKind::Image) {
                return a.path.clone().ok_or_else(|| {
                    Error::Compile(format!(
                        "qemu.disk_ref(\"{name}\"): image is declared but the scheduler \
                         produced no output path for it. (This typically means a build \
                         ordering bug — the image node should run before qemu launch.)"
                    ))
                });
            }
            if let Some(a) = find(ArtifactKind::BuildStep) {
                return a.path.clone().ok_or_else(|| {
                    Error::Compile(format!(
                        "qemu.disk_ref(\"{name}\"): build step is declared but the scheduler \
                         produced no output path for it. (This typically means a build \
                         ordering bug — the BuildStep node should run before qemu launch.)"
                    ))
                });
            }
            // Neither namespace resolved.
            let mut available: Vec<&str> = artifacts
                .iter()
                .filter(|a| matches!(a.kind, ArtifactKind::Image | ArtifactKind::BuildStep))
                .map(|a| a.name.as_str())
                .collect();
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

    const X86_TRIPLE: &str = "x86_64-unknown-none";

    fn none() -> Overrides {
        Overrides::default()
    }

    #[test]
    fn defaults_everywhere_when_unset() {
        let r = resolve_qemu(&QemuDef::default(), &none(), X86_TRIPLE).expect("resolve");
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
    }

    #[test]
    fn override_beats_qemu_def() {
        let qemu = QemuDef {
            memory_mb: Some(128),
            cores: Some(1),
            boot_mode: Some(BootMode::Direct),
            ..Default::default()
        };
        let o = Overrides {
            memory_mb: Some(512),
            cores: Some(4),
            boot_mode: Some(BootMode::Bios),
            ..none()
        };
        let r = resolve_qemu(&qemu, &o, X86_TRIPLE).expect("resolve");
        assert_eq!(r.memory_mb, 512);
        assert_eq!(r.cores, 4);
        assert_eq!(r.boot_mode, BootMode::Bios);
    }

    #[test]
    fn extra_args_append_in_order() {
        let qemu = QemuDef {
            extra_args: vec!["-display".into(), "none".into()],
            ..Default::default()
        };
        let o = Overrides {
            extra_args: vec!["-d".into(), "int".into()],
            ..none()
        };
        let r = resolve_qemu(&qemu, &o, X86_TRIPLE).expect("resolve");
        assert_eq!(r.extra_args, vec!["-display", "none", "-d", "int"]);
    }

    #[test]
    fn overrides_refuse_unknown_keys() {
        let err =
            serde_json::from_value::<Overrides>(serde_json::json!({"memory": 1})).unwrap_err();
        assert!(err.to_string().contains("unknown field `memory`"), "{err}");
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
        let r = resolve_qemu(&qemu, &none(), "sparc64-unknown-none")
            .expect("explicit binary should bypass target probe");
        assert_eq!(r.binary, "qemu-system-xtensa");
    }

    // -----------------------------------------------------------------
    // resolve_disk_source
    // -----------------------------------------------------------------

    fn artifact(name: &str, kind: ArtifactKind, path: Option<&str>) -> Artifact {
        Artifact {
            name: name.into(),
            kind,
            path: path.map(PathBuf::from),
        }
    }

    #[test]
    fn resolve_disk_image_returns_path_unchanged() {
        let disk = DiskSource::Image {
            path: PathBuf::from("/tmp/my.img"),
        };
        let p = resolve_disk_source(&disk, &[]).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/my.img"));
    }

    #[test]
    fn resolve_disk_ref_looks_up_image_path() {
        let artifacts = [artifact(
            "boot-disk",
            ArtifactKind::Image,
            Some("/build/images/boot-disk.img"),
        )];
        let disk = DiskSource::ImageRef {
            name: "boot-disk".into(),
        };
        let p = resolve_disk_source(&disk, &artifacts).unwrap();
        assert_eq!(p, PathBuf::from("/build/images/boot-disk.img"));
    }

    #[test]
    fn resolve_disk_ref_looks_up_build_step_path() {
        // disk_ref to a build step's name resolves through the
        // unified artifact namespace — the user can boot directly from
        // a build-step output without an intermediate image() wrapper.
        // An ESP of the same name is not a disk and is skipped.
        let artifacts = [
            artifact(
                "disk-image",
                ArtifactKind::Esp,
                Some("/build/esp/disk-image"),
            ),
            artifact(
                "disk-image",
                ArtifactKind::BuildStep,
                Some("/build/build-steps/disk-image/disk.img"),
            ),
        ];
        let disk = DiskSource::ImageRef {
            name: "disk-image".into(),
        };
        let p = resolve_disk_source(&disk, &artifacts).unwrap();
        assert_eq!(p, PathBuf::from("/build/build-steps/disk-image/disk.img"));
    }

    #[test]
    fn resolve_disk_ref_declared_but_unbuilt_is_an_ordering_error() {
        let artifacts = [artifact("boot-disk", ArtifactKind::Image, None)];
        let disk = DiskSource::ImageRef {
            name: "boot-disk".into(),
        };
        let msg = resolve_disk_source(&disk, &artifacts)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("produced no output path"), "{msg}");
    }

    #[test]
    fn resolve_disk_ref_unknown_image_lists_available() {
        let artifacts = [
            artifact("beta", ArtifactKind::Image, Some("/b")),
            artifact("alpha", ArtifactKind::Image, Some("/a")),
            artifact("esp", ArtifactKind::Esp, Some("/e")),
        ];
        let disk = DiskSource::ImageRef {
            name: "gamma".into(),
        };
        let msg = resolve_disk_source(&disk, &artifacts)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("gamma"));
        assert!(msg.contains("available: [alpha, beta]"), "{msg}");
    }
}
