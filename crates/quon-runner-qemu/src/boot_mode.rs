//! `BootModeAssembler` — per-mode QEMU argv contributions. Each
//! `BootMode` variant has an impl that knows how to wire its
//! firmware (or lack thereof), kernel, and disk/ESP into the
//! shared `args` vector.
//!
//! The coordinator (`qemu_cmd::build_qemu_command`) handles
//! everything mode-agnostic (memory, cores, display, serial,
//! test-mode exit, user extras). Mode-specific wiring
//! lives here.
//!
//! Adding a new boot mode means: (1) implement the trait,
//! (2) add an arm to `assembler_for`. The coordinator is
//! untouched.

use crate::error::{Error, Result};
use crate::ovmf::ResolvedOvmf;
use crate::resolve::ResolvedQemu;
use quon_model::{BootMode, EspSource};
use std::ffi::OsString;
use std::path::Path;

/// Append the mode-specific QEMU argv to `args`.
pub trait BootModeAssembler: Send + Sync {
    /// Assemble this mode's argv contribution.
    ///
    /// `args` is the shared output vector built by the
    /// coordinator; this method appends to it.
    /// `skip_path_checks` is set by the runner in `--dry-run` /
    /// `--no-build` modes and instructs the impl to skip
    /// `disk.exists()` / `esp.exists()` validation.
    fn assemble(
        &self,
        args: &mut Vec<OsString>,
        resolved: &ResolvedQemu,
        kernel: &Path,
        ovmf: Option<&ResolvedOvmf>,
        skip_path_checks: bool,
    ) -> Result<()>;
}

/// Direct-kernel boot: emit `-kernel <path>`. No firmware wiring,
/// no disk wiring — direct boot expects QEMU to load the kernel and
/// jump to its entry point itself.
pub struct DirectAssembler;

impl BootModeAssembler for DirectAssembler {
    fn assemble(
        &self,
        args: &mut Vec<OsString>,
        _resolved: &ResolvedQemu,
        kernel: &Path,
        _ovmf: Option<&ResolvedOvmf>,
        _skip_path_checks: bool,
    ) -> Result<()> {
        args.push("-kernel".into());
        args.push(kernel.as_os_str().to_os_string());
        Ok(())
    }
}

/// UEFI boot via OVMF pflash + a hybrid disk or ESP dir/image.
pub struct UefiAssembler;

impl BootModeAssembler for UefiAssembler {
    fn assemble(
        &self,
        args: &mut Vec<OsString>,
        resolved: &ResolvedQemu,
        _kernel: &Path,
        ovmf: Option<&ResolvedOvmf>,
        skip_path_checks: bool,
    ) -> Result<()> {
        // The coordinator (`build_qemu_command`) checks
        // `ovmf.is_some()` before dispatching to UefiAssembler;
        // if this expect ever fires, that invariant has been
        // broken upstream.
        let ovmf = ovmf.expect("uefi + ovmf invariant");
        push_pflash(args, &ovmf.code, true);
        push_pflash(args, &ovmf.vars, false);

        // Hybrid disk: a single raw drive that contains its own
        // ESP partition. OVMF probes attached drives for an EFI
        // System Partition automatically, so a generic `-drive`
        // (no `if=ide`) suffices. `disk` takes precedence over
        // `esp` — the user has named the disk explicitly, so any
        // auto-wired ESP from `model.esps` is intentionally
        // bypassed.
        if let Some(disk) = &resolved.disk {
            if !skip_path_checks && !disk.exists() {
                return Err(Error::EspMissing { path: disk.clone() });
            }
            args.push("-drive".into());
            let mut spec = OsString::from("format=raw,file=");
            spec.push(disk.as_os_str());
            args.push(spec);
        } else if let Some(esp) = &resolved.esp {
            // `skip_path_checks` is set by the runner when it's
            // in `--dry-run` or `--no-build` mode. In those
            // modes the ESP path may legitimately not exist yet
            // (dry-run skips the build; `--no-build` trusts the
            // user), and QEMU itself will produce a better error
            // at spawn time than we can from here.
            match esp {
                EspSource::Dir(dir) => {
                    if !skip_path_checks && !dir.exists() {
                        return Err(Error::EspMissing { path: dir.clone() });
                    }
                    args.push("-drive".into());
                    let mut spec = OsString::from("format=raw,file=fat:rw:");
                    spec.push(dir.as_os_str());
                    args.push(spec);
                }
                EspSource::Image(img) => {
                    if !skip_path_checks && !img.exists() {
                        return Err(Error::EspMissing { path: img.clone() });
                    }
                    args.push("-drive".into());
                    let mut spec = OsString::from("format=raw,file=");
                    spec.push(img.as_os_str());
                    args.push(spec);
                }
            }
        }
        Ok(())
    }
}

/// SeaBIOS boot: no `-kernel`, no pflash. Disk attaches as IDE0.
/// VVFAT (`EspSource::Dir`) is rejected — see the impl body for
/// the rationale.
pub struct BiosAssembler;

impl BootModeAssembler for BiosAssembler {
    fn assemble(
        &self,
        args: &mut Vec<OsString>,
        resolved: &ResolvedQemu,
        _kernel: &Path,
        _ovmf: Option<&ResolvedOvmf>,
        skip_path_checks: bool,
    ) -> Result<()> {
        // No `-kernel` (SeaBIOS won't use it), no pflash (no OVMF).
        // QEMU's default firmware for `pc`/`q35` is SeaBIOS; we
        // deliberately do NOT emit `-bios` so users on machines
        // with a different default (or a non-x86 host) aren't
        // forced into a path that doesn't apply.
        //
        // The boot disk comes from `resolved.disk` first (the
        // mode-agnostic surface). Falls back to `resolved.esp` for
        // back-compat — the BIOS-mode `EspSource::Image` path
        // predates `DiskSource` and is kept to avoid breaking
        // existing configs.
        //
        // VVFAT (`EspSource::Dir`) is rejected: the VVFAT driver
        // synthesises a FAT filesystem at the partition level and
        // doesn't expose an MBR / boot sector, so SeaBIOS can't
        // find anything to boot. The user must build a real disk
        // image with `Image` or pre-supply one with `Qemu::disk_file`.
        if let Some(disk) = &resolved.disk {
            if !skip_path_checks && !disk.exists() {
                return Err(Error::EspMissing { path: disk.clone() });
            }
            args.push("-drive".into());
            let mut spec = OsString::from("format=raw,file=");
            spec.push(disk.as_os_str());
            spec.push(",if=ide,index=0");
            args.push(spec);
        } else if let Some(esp) = &resolved.esp {
            match esp {
                EspSource::Dir(_) => {
                    return Err(Error::Config(
                        "BootMode::Bios with Qemu::esp_dir(..) is not supported: \
                         QEMU's VVFAT driver does not expose an MBR/boot-sector for \
                         SeaBIOS to read. Use Qemu::esp_image(<raw disk path>) or build a \
                         hybrid disk with Image."
                            .into(),
                    ));
                }
                EspSource::Image(img) => {
                    if !skip_path_checks && !img.exists() {
                        return Err(Error::EspMissing { path: img.clone() });
                    }
                    args.push("-drive".into());
                    let mut spec = OsString::from("format=raw,file=");
                    spec.push(img.as_os_str());
                    spec.push(",if=ide,index=0");
                    args.push(spec);
                }
            }
        }
        Ok(())
    }
}

/// Resolve a `BootMode` to its assembler. Static dispatch — one
/// arm per variant; a missing arm is a compile error.
pub fn assembler_for(mode: BootMode) -> &'static dyn BootModeAssembler {
    match mode {
        BootMode::Direct => &DirectAssembler,
        BootMode::Uefi => &UefiAssembler,
        BootMode::Bios => &BiosAssembler,
    }
}

/// Append a pflash drive (`-drive if=pflash,...`) for OVMF code
/// or vars.
///
/// `readonly` should be `true` for OVMF code (it's a ROM image —
/// QEMU may map it directly without a writable copy) and `false`
/// for OVMF vars (NVRAM mutates across boots).
fn push_pflash(args: &mut Vec<OsString>, path: &Path, readonly: bool) {
    args.push("-drive".into());
    let mut spec = OsString::from(if readonly {
        "if=pflash,format=raw,readonly=on,file="
    } else {
        "if=pflash,format=raw,file="
    });
    spec.push(path.as_os_str());
    args.push(spec);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ovmf::ResolvedOvmf;
    use quon_model::{BootMode, EspSource, SerialMode};
    use std::path::PathBuf;

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn stub_resolved() -> ResolvedQemu {
        ResolvedQemu {
            binary: "qemu-system-x86_64".into(),
            machine: "q35".into(),
            memory_mb: 256,
            cores: 1,
            serial: SerialMode::Stdio,
            display: None,
            extra_args: Vec::new(),
            boot_mode: BootMode::Direct,
            ovmf_code: None,
            ovmf_vars: None,
            esp: None,
            disk: None,
            test_exit_port: 0xf4,
            test_success_code: 0x10,
        }
    }

    // -----------------------------------------------------------------
    // DirectAssembler tests
    // -----------------------------------------------------------------

    #[test]
    fn direct_mode_basic_argv() {
        let resolved = stub_resolved();
        let kernel = PathBuf::from("/tmp/build/kernel");
        let mut args: Vec<OsString> = Vec::new();
        DirectAssembler
            .assemble(&mut args, &resolved, &kernel, None, false)
            .unwrap();
        let a = args_to_strings(&args);
        assert_eq!(a, vec!["-kernel", "/tmp/build/kernel"]);
    }

    // -----------------------------------------------------------------
    // UefiAssembler tests
    // -----------------------------------------------------------------

    #[test]
    fn uefi_mode_emits_pflash_pair() {
        let r = stub_resolved();
        let ovmf = ResolvedOvmf {
            code: PathBuf::from("/opt/ovmf/CODE.fd"),
            vars: PathBuf::from("/tmp/build/VARS.fd"),
        };
        let mut args: Vec<OsString> = Vec::new();
        UefiAssembler
            .assemble(&mut args, &r, Path::new("/ignored"), Some(&ovmf), false)
            .unwrap();
        let a = args_to_strings(&args);
        let code_arg = "if=pflash,format=raw,readonly=on,file=/opt/ovmf/CODE.fd";
        let vars_arg = "if=pflash,format=raw,file=/tmp/build/VARS.fd";
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == code_arg),
            "expected readonly code pflash, got {a:?}"
        );
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == vars_arg),
            "expected writable vars pflash, got {a:?}"
        );
        // Direct-mode -kernel flag must NOT appear in UEFI mode.
        assert!(!a.iter().any(|s| s == "-kernel"));
    }

    #[test]
    fn uefi_mode_drive_spec_per_source() {
        // The disk case pins a plain raw drive with no `if=ide`: OVMF
        // probes any attached disk for an EFI System Partition.
        type Configure = fn(&mut ResolvedQemu, PathBuf);
        let cases: [(&str, &str, Configure, &str); 3] = [
            (
                "esp dir",
                "esp",
                |r, p| r.esp = Some(EspSource::Dir(p)),
                "format=raw,file=fat:rw:",
            ),
            (
                "esp image",
                "disk.img",
                |r, p| r.esp = Some(EspSource::Image(p)),
                "format=raw,file=",
            ),
            (
                "disk",
                "hybrid.img",
                |r, p| r.disk = Some(p),
                "format=raw,file=",
            ),
        ];
        for (name, file, configure, prefix) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let path = tmp.path().join(file);
            if name == "esp dir" {
                std::fs::create_dir_all(&path).unwrap();
            } else {
                std::fs::write(&path, b"stub").unwrap();
            }

            let mut r = stub_resolved();
            r.boot_mode = BootMode::Uefi;
            configure(&mut r, path.clone());
            let ovmf = ResolvedOvmf {
                code: PathBuf::from("/a"),
                vars: PathBuf::from("/b"),
            };
            let mut args: Vec<OsString> = Vec::new();
            UefiAssembler
                .assemble(&mut args, &r, Path::new("/k"), Some(&ovmf), false)
                .unwrap();
            let a = args_to_strings(&args);
            let expected = format!("{prefix}{}", path.display());
            assert!(
                a.windows(2).any(|w| w[0] == "-drive" && w[1] == expected),
                "{name}: expected -drive {expected}, got {a:?}"
            );
        }
    }

    #[test]
    fn esp_dir_missing_errors() {
        let mut r = stub_resolved();
        r.boot_mode = BootMode::Uefi;
        r.esp = Some(EspSource::Dir(PathBuf::from("/nonexistent/esp/abc123")));
        let ovmf = ResolvedOvmf {
            code: PathBuf::from("/a"),
            vars: PathBuf::from("/b"),
        };
        let mut args: Vec<OsString> = Vec::new();
        let err = UefiAssembler
            .assemble(&mut args, &r, Path::new("/k"), Some(&ovmf), false)
            .unwrap_err();
        assert!(matches!(err, Error::EspMissing { .. }));
    }

    // -----------------------------------------------------------------
    // BiosAssembler tests
    // -----------------------------------------------------------------
    //
    // BIOS mode is the "let SeaBIOS run from a raw disk" path. Three
    // invariants matter:
    //
    //   1. No `-kernel` (would short-circuit BIOS firmware).
    //   2. No pflash (no OVMF — that's the UEFI path).
    //   3. The raw disk attaches as `if=ide,index=0` so SeaBIOS
    //      finds it as the first boot device.
    //
    // VVFAT (`EspSource::Dir`) is rejected because the synthesised
    // FAT volume has no MBR for SeaBIOS to read.

    #[test]
    fn bios_mode_no_kernel_no_pflash() {
        let r = stub_resolved();
        let mut args: Vec<OsString> = Vec::new();
        BiosAssembler
            .assemble(&mut args, &r, Path::new("/ignored"), None, false)
            .unwrap();
        let a = args_to_strings(&args);
        assert!(
            !a.iter().any(|s| s == "-kernel"),
            "BIOS mode must not emit -kernel, got: {a:?}"
        );
        assert!(
            !a.iter().any(|s| s.contains("pflash")),
            "BIOS mode must not emit pflash, got: {a:?}"
        );
    }

    #[test]
    fn bios_mode_with_image_disk_attaches_as_ide0() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("disk.img");
        std::fs::write(&img, b"stub").unwrap();

        let mut r = stub_resolved();
        r.boot_mode = BootMode::Bios;
        r.esp = Some(EspSource::Image(img.clone()));
        let mut args: Vec<OsString> = Vec::new();
        BiosAssembler
            .assemble(&mut args, &r, Path::new("/k"), None, false)
            .unwrap();
        let a = args_to_strings(&args);
        let expected = format!("format=raw,file={},if=ide,index=0", img.display());
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == expected),
            "expected raw IDE drive for {}, got {a:?}",
            img.display()
        );
    }

    #[test]
    fn bios_mode_rejects_esp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("esp");
        std::fs::create_dir_all(&dir).unwrap();

        let mut r = stub_resolved();
        r.boot_mode = BootMode::Bios;
        r.esp = Some(EspSource::Dir(dir));
        let mut args: Vec<OsString> = Vec::new();
        let err = BiosAssembler
            .assemble(&mut args, &r, Path::new("/k"), None, false)
            .unwrap_err();
        assert!(
            matches!(err, Error::Config(ref msg) if msg.contains("VVFAT")),
            "expected Config error mentioning VVFAT, got: {err:?}"
        );
    }

    #[test]
    fn bios_mode_with_disk_takes_precedence_over_esp() {
        let tmp = tempfile::tempdir().unwrap();
        let disk = tmp.path().join("hybrid.img");
        std::fs::write(&disk, b"stub").unwrap();
        let esp_img = tmp.path().join("legacy.img");
        std::fs::write(&esp_img, b"legacy").unwrap();

        let mut r = stub_resolved();
        r.boot_mode = BootMode::Bios;
        r.disk = Some(disk.clone());
        r.esp = Some(EspSource::Image(esp_img.clone()));
        let mut args: Vec<OsString> = Vec::new();
        BiosAssembler
            .assemble(&mut args, &r, Path::new("/k"), None, false)
            .unwrap();
        let a = args_to_strings(&args);
        let expected = format!("format=raw,file={},if=ide,index=0", disk.display());
        assert!(
            a.windows(2).any(|w| w[0] == "-drive" && w[1] == expected),
            "expected disk to take precedence, got: {a:?}"
        );
        // Legacy esp_image must NOT also be attached.
        let legacy = format!("format=raw,file={}", esp_img.display());
        assert!(
            !a.iter().any(|s| s.as_str() == legacy.as_str()),
            "legacy esp must not appear when disk is set, got: {a:?}"
        );
    }

    #[test]
    fn bios_mode_without_esp_emits_no_drive() {
        let r = stub_resolved();
        let mut args: Vec<OsString> = Vec::new();
        BiosAssembler
            .assemble(&mut args, &r, Path::new("/k"), None, false)
            .unwrap();
        let a = args_to_strings(&args);
        // No -drive flags should be present at all when no esp is set.
        // The user is expected to supply a drive via extra_args.
        assert!(
            !a.iter().any(|s| s == "-drive"),
            "BIOS mode without esp must not emit a managed -drive, got: {a:?}"
        );
    }
}
