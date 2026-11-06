//! `quon run` subcommand: builds the project and starts the profile's
//! run binary with its runner.
//!
//! See [`mod@quon_core::run`] for the actual pipeline; this module is a
//! thin adapter between [`crate::cli::RunArgs`] and
//! [`quon_core::RunOptions`].

use super::CmdContext;
use crate::cli::RunArgs;
use crate::output::{BuildReporter, Counts, Shell, elapsed};
use anyhow::{Result, bail};
use quon_core::model::BootMode;
use std::path::Path;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

/// Execute the `quon run` pipeline against the prepared context.
pub fn run(
    ctx: CmdContext,
    jobs: Option<usize>,
    args: RunArgs,
    shell: &mut Shell,
    cwd: &Path,
) -> Result<()> {
    // CLI boot-mode override, passed to a plugin runner as its
    // `boot_mode` override. The three flags are mutually exclusive in
    // the clap schema; if none are set, we pass `None` so the
    // runner's own `.boot(...)` wins.
    let boot_mode_override = match (args.uefi, args.direct, args.bios) {
        (true, false, false) => Some(BootMode::Uefi),
        (false, true, false) => Some(BootMode::Direct),
        (false, false, true) => Some(BootMode::Bios),
        (false, false, false) => None,
        _ => unreachable!("clap enforces mutual exclusion"),
    };

    let opts = quon_core::RunOptions {
        boot_mode_override,
        timeout_override: args.timeout.map(Duration::from_secs),
        extra_args: args.extra,
        workers: jobs,
        dry_run: args.dry_run,
        no_build: args.no_build,
        gdb: args.gdb,
        test: false,
        runners: super::runners(),
    };

    // `prepare` checks the runner before the build, so a profile
    // without one, or with bad runner settings, fails fast instead of
    // after minutes of compiling.
    let mut reporter = BuildReporter::new(shell, ctx.ctx.driver());
    let prepared =
        quon_core::run::prepare(&ctx.ctx, &ctx.model, &ctx.resolved, &opts, &mut reporter)?;
    // Erases the progress line before anything else prints.
    drop(reporter);
    let command = prepared.invocation.command_line();

    if args.dry_run {
        // stdout, so scripts and tests can capture the command alone.
        println!("{command}");
        return Ok(());
    }
    if let Some(summary) = &prepared.build {
        shell.finished(
            &ctx.resolved.profile,
            Counts {
                noun: "built",
                built: summary.built,
                cached: summary.cached,
                copies: 0,
            },
        );
    }
    // Printed before the spawn: a runner says, for example, that QEMU
    // halts waiting for the debugger and prints nothing until one
    // connects.
    for note in &prepared.invocation.notes {
        shell.note(note);
    }
    // The line marks where Quon's output ends and the guest's starts.
    // Paths under the current directory are shown relative to it, as
    // Cargo shows them; `--dry-run` above keeps them absolute.
    let shown = command.replace(&format!("{}/", cwd.display()), "");
    shell.status("Running", format_args!("`{shown}`"));
    let started = Instant::now();
    let program = format!("runner '{}'", prepared.invocation.runner);
    let success_exit_code = prepared.invocation.success_exit_code;
    let status = quon_core::run::launch(prepared.invocation)?;

    check_exit_status(&program, status, success_exit_code)?;
    // A failure is reported by the error instead.
    shell.status(
        "Exited",
        format_args!(
            "{program} with status {} after {}",
            status.code().unwrap_or(0),
            elapsed(started.elapsed())
        ),
    );
    Ok(())
}

/// Map the runner's exit status to the result of `quon run`.
///
/// With `success_exit_code` unset, only status 0 succeeds. With it set,
/// only that status succeeds, so a kernel that leaves through
/// `isa-debug-exit` can report success and every other status,
/// including a plain 0 from a guest power-off, is a failure.
fn check_exit_status(
    program: &str,
    status: ExitStatus,
    success_exit_code: Option<i32>,
) -> Result<()> {
    // Distinguish three non-success cases so the user gets a
    // diagnostic that tells them what actually happened:
    //
    // 1. Signal-terminated QEMU (e.g. user hit Ctrl-C and SIGINT
    //    made it to QEMU directly rather than to quon). On Unix,
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
            bail!("{program} killed by signal {sig}");
        }
    }
    let code = status.code().unwrap_or(-1);
    match success_exit_code {
        None if code == 0 => Ok(()),
        Some(expected) if code == expected => Ok(()),
        Some(expected) => {
            bail!("{program} exited with status {code}, expected success status {expected}")
        }
        None => bail!("{program} exited with status {code}"),
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
        assert!(check_exit_status("runner 'qemu'", exited(0), None).is_ok());
    }

    #[test]
    fn nonzero_fails_without_success_code() {
        let err = check_exit_status("runner 'qemu'", exited(33), None).unwrap_err();
        assert_eq!(err.to_string(), "runner 'qemu' exited with status 33");
    }

    #[test]
    fn success_code_succeeds() {
        assert!(check_exit_status("runner 'qemu'", exited(33), Some(33)).is_ok());
    }

    #[test]
    fn other_status_fails_with_success_code() {
        let err = check_exit_status("runner 'qemu'", exited(35), Some(33)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "runner 'qemu' exited with status 35, expected success status 33"
        );
    }

    #[test]
    fn zero_fails_with_success_code() {
        assert!(check_exit_status("runner 'qemu'", exited(0), Some(33)).is_err());
    }

    #[test]
    fn signal_fails_with_success_code() {
        // SIGKILL: the low 7 bits of a raw wait status hold the signal.
        let err =
            check_exit_status("runner 'qemu'", ExitStatus::from_raw(9), Some(33)).unwrap_err();
        assert_eq!(err.to_string(), "runner 'qemu' killed by signal 9");
    }
}
