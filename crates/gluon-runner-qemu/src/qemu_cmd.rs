//! Pure QEMU argv assembly.
//!
//! `build_qemu_command` is a deterministic, side-effect-free function
//! that takes a [`ResolvedQemu`] plus the boot-mode inputs and produces
//! a [`QemuCommand`] (binary + args). Keeping it pure
//! means:
//!
//! - Unit tests for argv composition never need a real filesystem or a
//!   real OVMF install.
//! - The `gluon run --dry-run` path can print the exact argv that
//!   would be used without needing QEMU at all.
//! - `gluon test` reuses it for each test's argv while managing the
//!   surrounding process lifecycle itself.
//!
//! All "live" work (resolving OVMF paths, copying a writable vars file)
//! lives in [`crate::ovmf`]. Gluon spawns the command.

use crate::boot_mode;
use crate::error::{Error, Result};
use crate::ovmf::ResolvedOvmf;
use crate::resolve::ResolvedQemu;
use gluon_model::{BootMode, DisplayMode, SerialMode};
use std::ffi::OsString;
use std::path::Path;

/// The QEMU program and its arguments.
#[derive(Debug, Clone)]
pub struct QemuCommand {
    pub binary: String,
    pub args: Vec<OsString>,
}

/// Assemble the final QEMU argv.
///
/// `kernel` is the path that gets fed to `-kernel` in direct mode. In
/// UEFI mode it is unused by QEMU itself (UEFI loads a PE32+ image
/// from the ESP) but we still carry it for diagnostic messages.
///
/// `ovmf` **must** be `Some` when `mode == BootMode::Uefi` and `None`
/// otherwise. The caller is responsible for running
/// [`crate::ovmf::resolve_ovmf`] and any vars-writability copy before
/// calling this function.
///
/// Errors:
/// - [`Error::Config`] if `mode == Uefi` but `ovmf` is `None`.
/// - [`Error::EspMissing`] if the ESP source file/directory does not
///   exist on disk. This is the only filesystem touch in this function;
///   it is done because letting QEMU fail 200ms later with a cryptic
///   "no bootable device" is a worse experience than a targeted error.
pub fn build_qemu_command(
    resolved: &ResolvedQemu,
    kernel: &Path,
    mode: BootMode,
    ovmf: Option<&ResolvedOvmf>,
    extra_cli_args: &[OsString],
    test_mode: bool,
    skip_path_checks: bool,
) -> Result<QemuCommand> {
    if mode == BootMode::Uefi && ovmf.is_none() {
        return Err(Error::Config(
            "internal: build_qemu_command(UEFI) called without a resolved OVMF firmware".into(),
        ));
    }

    let mut args: Vec<OsString> = Vec::new();

    // 1. Machine type.
    args.push("-machine".into());
    args.push(resolved.machine.clone().into());

    // 2. Memory.
    args.push("-m".into());
    args.push(format!("{}M", resolved.memory_mb).into());

    // 3. SMP.
    args.push("-smp".into());
    args.push(resolved.cores.to_string().into());

    // 4. Display policy.
    //
    // Precedence (highest first):
    //   a. User passed `-display` via `extra_args` or CLI pass-through —
    //      respect their literal flag and skip everything else here.
    //   b. `qemu().display(...)` is set — emit the matching variant.
    //      `Default` deliberately omits `-display` so QEMU picks its
    //      compiled-in backend (cocoa on macOS, gtk/sdl on Linux). Use
    //      this when the user wants a window without naming the backend.
    //   c. Otherwise fall back to `-display none` so headless / CI
    //      runs (the dominant case historically) keep working with no
    //      config change.
    if !user_has_flag(&resolved.extra_args, extra_cli_args, "-display") {
        match &resolved.display {
            Some(DisplayMode::Default) => { /* omit -display, QEMU picks */ }
            Some(DisplayMode::None) => {
                args.push("-display".into());
                args.push("none".into());
            }
            Some(DisplayMode::Spec(spec)) => {
                args.push("-display".into());
                args.push(spec.clone().into());
            }
            None => {
                args.push("-display".into());
                args.push("none".into());
            }
        }
    }

    // 5. Serial policy. Skip if the user set an explicit `-serial`.
    if !user_has_flag(&resolved.extra_args, extra_cli_args, "-serial") {
        match &resolved.serial {
            SerialMode::Stdio => {
                args.push("-serial".into());
                args.push("stdio".into());
            }
            SerialMode::None => {
                args.push("-serial".into());
                args.push("none".into());
            }
            SerialMode::File(path) => {
                args.push("-serial".into());
                let mut spec = OsString::from("file:");
                spec.push(path.as_os_str());
                args.push(spec);
            }
        }
    }

    // 6. Mode-specific boot wiring — delegated to BootModeAssembler.
    boot_mode::assembler_for(mode).assemble(&mut args, resolved, kernel, ovmf, skip_path_checks)?;

    // 7. Test-mode exit device. Wired in here (before user extras)
    //    so the user can still override or suppress via `extra_args`
    //    if they really want to, but positioned ahead of the CLI
    //    pass-through so `gluon test -- <extras>` doesn't clobber
    //    it by accident. The port + iosize combo matches the
    //    convention documented in the Rust embedonomicon for
    //    bare-metal test exit; iosize is
    //    fixed at 0x04 because QEMU's isa-debug-exit device takes
    //    a 32-bit write at that offset.
    if test_mode {
        args.push("-device".into());
        args.push(
            format!(
                "isa-debug-exit,iobase=0x{:x},iosize=0x04",
                resolved.test_exit_port
            )
            .into(),
        );
    }

    // 8. User's qemu.extra_args (from Rhai).
    for a in &resolved.extra_args {
        args.push(a.into());
    }

    // 9. CLI pass-through (`gluon run -- <extra>`).
    args.extend(extra_cli_args.iter().cloned());

    Ok(QemuCommand {
        binary: resolved.binary.clone(),
        args,
    })
}

/// Whether any of the user-provided argument lists contain `flag`.
///
/// Checks both the script-level `extra_args` and the `gluon run -- ...`
/// CLI pass-through. Used by the default `-display none` / `-serial
/// stdio` insertion logic so users can override either with a bare
/// `-display gtk` in their config or on the command line.
fn user_has_flag(script_extras: &[String], cli_extras: &[OsString], flag: &str) -> bool {
    script_extras.iter().any(|a| a == flag)
        || cli_extras.iter().any(|a| a.to_string_lossy() == flag)
}

/// Suggestive test helper: a `ResolvedQemu` that the caller customises.
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn direct_mode_custom_memory_and_cores() {
        let mut r = stub_resolved();
        r.memory_mb = 1024;
        r.cores = 4;
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &[],
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        assert!(a.windows(2).any(|w| w == ["-m", "1024M"]));
        assert!(a.windows(2).any(|w| w == ["-smp", "4"]));
    }

    #[test]
    fn uefi_mode_requires_ovmf() {
        let mut r = stub_resolved();
        r.boot_mode = BootMode::Uefi;
        let err = build_qemu_command(&r, Path::new("/k"), BootMode::Uefi, None, &[], false, false)
            .unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn user_display_override_skips_default_display_none() {
        let mut r = stub_resolved();
        r.extra_args = vec!["-display".into(), "gtk".into()];
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &[],
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        // Default "none" must not be injected when user specified -display.
        let idx = a.iter().position(|s| s == "-display").unwrap();
        assert_eq!(a[idx + 1], "gtk");
        assert!(!a.iter().any(|s| s == "none"), "unexpected: {a:?}");
    }

    #[test]
    fn user_cli_display_override_also_skips_default() {
        let r = stub_resolved();
        let extra = [os("-display"), os("curses")];
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &extra,
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        // Default -display none should NOT appear; the user's -display
        // curses is appended at the end via cli_extras.
        let display_positions: Vec<_> = a
            .iter()
            .enumerate()
            .filter(|(_, s)| *s == "-display")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(display_positions.len(), 1, "{a:?}");
    }

    #[test]
    fn display_default_omits_flag_so_qemu_picks_backend() {
        // `qemu().display("default")` → DisplayMode::Default → no
        // `-display` flag at all. QEMU then uses its compiled-in
        // default backend (cocoa on macOS, gtk/sdl on Linux).
        let mut r = stub_resolved();
        r.display = Some(DisplayMode::Default);
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &[],
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        assert!(
            !a.iter().any(|s| s == "-display"),
            "DisplayMode::Default must not emit -display, got {a:?}"
        );
    }

    #[test]
    fn display_and_serial_modes_emit_their_flag_value() {
        // Explicit `DisplayMode::None` emits the same flag as the implicit
        // default; keeping it here stops a refactor from dropping the
        // explicit branch.
        type Configure = fn(&mut ResolvedQemu);
        let cases: [(&str, Configure, &str, &str); 4] = [
            (
                "display spec",
                |r| r.display = Some(DisplayMode::Spec("cocoa".into())),
                "-display",
                "cocoa",
            ),
            (
                "explicit display none",
                |r| r.display = Some(DisplayMode::None),
                "-display",
                "none",
            ),
            (
                "serial none",
                |r| r.serial = SerialMode::None,
                "-serial",
                "none",
            ),
            (
                "serial file",
                |r| r.serial = SerialMode::File(PathBuf::from("/tmp/out.log")),
                "-serial",
                "file:/tmp/out.log",
            ),
        ];
        for (name, configure, flag, expected) in cases {
            let mut r = stub_resolved();
            configure(&mut r);
            let inv = build_qemu_command(
                &r,
                Path::new("/k"),
                BootMode::Direct,
                None,
                &[],
                false,
                false,
            )
            .unwrap();
            let a = args_to_strings(&inv.args);
            let idx = a
                .iter()
                .position(|s| s == flag)
                .unwrap_or_else(|| panic!("{name}: no {flag} flag in {a:?}"));
            assert_eq!(a[idx + 1], expected, "{name}");
        }
    }

    #[test]
    fn user_extra_args_display_override_wins_over_qemu_display() {
        // Precedence (a) in qemu_cmd:84-comment: even when the user set
        // qemu().display(...), an explicit -display in extra_args wins.
        let mut r = stub_resolved();
        r.display = Some(DisplayMode::Spec("cocoa".into()));
        r.extra_args = vec!["-display".into(), "gtk".into()];
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &[],
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        // Only one -display flag (the user's). Gluon must NOT also
        // append -display cocoa for DisplayMode::Spec.
        let positions: Vec<_> = a
            .iter()
            .enumerate()
            .filter(|(_, s)| *s == "-display")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(positions.len(), 1, "{a:?}");
        assert_eq!(a[positions[0] + 1], "gtk");
    }

    #[test]
    fn user_extra_args_ordering() {
        let mut r = stub_resolved();
        r.extra_args = vec!["-d".into(), "int".into()];
        let extra = [os("-no-reboot")];
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &extra,
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        // The kernel flag is part of gluon's managed section;
        // user args (script then cli) come after it in order.
        let k = a.iter().position(|s| s == "-kernel").unwrap();
        let d = a.iter().position(|s| s == "-d").unwrap();
        let nr = a.iter().position(|s| s == "-no-reboot").unwrap();
        assert!(k < d && d < nr, "order broken: {a:?}");
    }

    #[test]
    fn test_mode_off_omits_isa_debug_exit() {
        let r = stub_resolved();
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &[],
            false,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        assert!(
            !a.iter().any(|s| s.contains("isa-debug-exit")),
            "test_mode=false must not emit isa-debug-exit, got: {a:?}"
        );
    }

    #[test]
    fn test_mode_on_emits_isa_debug_exit_with_resolved_port() {
        let mut r = stub_resolved();
        // Not the stub's 0xf4 default, so the spec proves the field is read.
        r.test_exit_port = 0x501;
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &[],
            true,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        // Expect the pair `-device isa-debug-exit,iobase=0x501,iosize=0x04`.
        let pair: Vec<_> = a
            .windows(2)
            .filter(|w| w[0] == "-device" && w[1].starts_with("isa-debug-exit"))
            .collect();
        assert_eq!(
            pair.len(),
            1,
            "expected exactly one -device entry, got {a:?}"
        );
        assert_eq!(
            pair[0][1], "isa-debug-exit,iobase=0x501,iosize=0x04",
            "exit device spec mismatch"
        );
    }

    #[test]
    fn test_mode_device_precedes_user_extra_args() {
        // The managed isa-debug-exit entry must come BEFORE the
        // user's Rhai-level extra_args, so a user who wants to
        // override or augment the test device can append another
        // -device entry and trust the order.
        let mut r = stub_resolved();
        r.extra_args = vec!["-d".into(), "int".into()];
        let inv = build_qemu_command(
            &r,
            Path::new("/k"),
            BootMode::Direct,
            None,
            &[],
            true,
            false,
        )
        .unwrap();
        let a = args_to_strings(&inv.args);
        let pos_device = a
            .iter()
            .position(|s| s.starts_with("isa-debug-exit"))
            .unwrap();
        let pos_d = a.iter().position(|s| s == "-d").unwrap();
        assert!(
            pos_device < pos_d,
            "managed -device must precede user extras: {a:?}"
        );
    }
}
