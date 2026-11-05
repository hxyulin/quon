//! `gluon run` subcommand — builds the project and launches QEMU.
//!
//! See [`mod@gluon_core::run`] for the actual pipeline; this module is a
//! thin adapter between [`crate::cli::RunArgs`] and
//! [`gluon_core::RunOptions`].

use super::CmdContext;
use crate::cli::RunArgs;
use crate::output::{BuildReporter, Shell};
use anyhow::{Result, bail};
use gluon_core::Error as CoreError;
use gluon_core::model::BootMode;
use std::process::ExitStatus;
use std::time::Duration;

/// Execute the `gluon run` pipeline against the prepared context.
pub fn run(ctx: CmdContext, jobs: Option<usize>, args: RunArgs, shell: &mut Shell) -> Result<()> {
    // Reject an absent qemu block at the CLI boundary so the user gets
    // a targeted diagnostic ("you forgot a `cfg.qemu(...)` block")
    // instead of a panic from inside `gluon_core::run`. `model.qemu`
    // is `Option<QemuDef>` since Stage 4 Phase G — the runner inside
    // gluon-core relies on the CLI to do this gating before calling in.
    if ctx.model.qemu.is_none() {
        return Err(CoreError::QemuNotConfigured.into());
    }

    // CLI boot-mode override. The three flags are mutually exclusive
    // in the clap schema; if none are set, we pass `None` so the
    // profile's own `qemu_boot_mode(...)` (or project-level
    // `qemu().boot_mode(...)`) wins.
    let boot_mode_override = match (args.uefi, args.direct, args.bios) {
        (true, false, false) => Some(BootMode::Uefi),
        (false, true, false) => Some(BootMode::Direct),
        (false, false, true) => Some(BootMode::Bios),
        (false, false, false) => None,
        _ => unreachable!("clap enforces mutual exclusion"),
    };

    let opts = gluon_core::RunOptions {
        boot_mode_override,
        timeout_override: args.timeout.map(Duration::from_secs),
        extra_args: args.extra,
        workers: jobs,
        dry_run: args.dry_run,
        no_build: args.no_build,
        gdb: args.gdb,
        // Not exposed to `gluon run` — reserved for the future
        // `gluon test` subcommand.
        test_mode: false,
    };

    let mut reporter = BuildReporter::new(shell, ctx.ctx.driver());
    let status = gluon_core::run(&ctx.ctx, &ctx.model, &ctx.resolved, opts, &mut reporter)?;

    if args.dry_run {
        return Ok(());
    }
    let success_exit_code = ctx.model.qemu.as_ref().and_then(|q| q.success_exit_code);
    check_exit_status(status, success_exit_code)
}

/// Map QEMU's exit status to the result of `gluon run`.
///
/// With `success_exit_code` unset, only status 0 succeeds. With it set,
/// only that status succeeds, so a kernel that leaves through
/// `isa-debug-exit` can report success and every other status,
/// including a plain 0 from a guest power-off, is a failure.
fn check_exit_status(status: ExitStatus, success_exit_code: Option<i32>) -> Result<()> {
    // Distinguish three non-success cases so the user gets a
    // diagnostic that tells them what actually happened:
    //
    // 1. Signal-terminated QEMU (e.g. user hit Ctrl-C and SIGINT
    //    made it to QEMU directly rather than to gluon). On Unix,
    //    `ExitStatus::signal()` returns `Some(n)` in this case and
    //    `code()` returns `None`; on Windows the concept doesn't
    //    exist and this branch is skipped.
    // 2. QEMU exited with a numeric code — pass it through verbatim.
    // 3. Neither of the above (shouldn't happen on a well-formed
    //    wait result, but we handle it defensively).
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            bail!("QEMU killed by signal {sig}");
        }
    }
    let code = status.code().unwrap_or(-1);
    match success_exit_code {
        None if code == 0 => Ok(()),
        Some(expected) if code == expected => Ok(()),
        Some(expected) => {
            bail!("QEMU exited with status {code}, expected success status {expected}")
        }
        None => bail!("QEMU exited with status {code}"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::check_exit_status;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// Build the status a process that called `exit(code)` reports.
    fn exited(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn zero_succeeds_without_success_code() {
        assert!(check_exit_status(exited(0), None).is_ok());
    }

    #[test]
    fn nonzero_fails_without_success_code() {
        let err = check_exit_status(exited(33), None).unwrap_err();
        assert_eq!(err.to_string(), "QEMU exited with status 33");
    }

    #[test]
    fn success_code_succeeds() {
        assert!(check_exit_status(exited(33), Some(33)).is_ok());
    }

    #[test]
    fn other_status_fails_with_success_code() {
        let err = check_exit_status(exited(35), Some(33)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "QEMU exited with status 35, expected success status 33"
        );
    }

    #[test]
    fn zero_fails_with_success_code() {
        assert!(check_exit_status(exited(0), Some(33)).is_err());
    }

    #[test]
    fn signal_fails_with_success_code() {
        // SIGKILL: the low 7 bits of a raw wait status hold the signal.
        let err = check_exit_status(ExitStatus::from_raw(9), Some(33)).unwrap_err();
        assert_eq!(err.to_string(), "QEMU killed by signal 9");
    }
}
