//! `gluon test` subcommand: builds the integration tests and boots each
//! one. See [`gluon_core::run::prepare_tests`] for the pipeline; this
//! module maps [`crate::cli::TestArgs`] to [`gluon_core::TestOptions`],
//! boots the tests and prints the results.

use super::CmdContext;
use crate::cli::TestArgs;
use crate::output::{BuildReporter, Counts, Shell};
use anyhow::{Result, bail};
use gluon_core::model::BootMode;
use gluon_core::{Error as CoreError, TestStatus};
use std::time::Duration;

/// Execute the `gluon test` pipeline against the prepared context.
pub fn run(ctx: CmdContext, jobs: Option<usize>, args: TestArgs, shell: &mut Shell) -> Result<()> {
    if ctx.model.qemu.is_none() {
        return Err(CoreError::QemuNotConfigured.into());
    }
    let boot_mode_override = match (args.uefi, args.direct) {
        (true, _) => Some(BootMode::Uefi),
        (_, true) => Some(BootMode::Direct),
        _ => None,
    };
    let opts = gluon_core::TestOptions {
        boot_mode_override,
        timeout_override: args.timeout.map(Duration::from_secs),
        extra_args: args.extra,
        workers: jobs,
        filter: args.filter,
        dry_run: args.dry_run,
        no_build: args.no_build,
    };

    let mut reporter = BuildReporter::new(shell, ctx.ctx.driver());
    let prepared =
        gluon_core::run::prepare_tests(&ctx.ctx, &ctx.model, &ctx.resolved, &opts, &mut reporter)?;
    // Erases the progress line before anything else prints.
    drop(reporter);

    if args.dry_run {
        // stdout, so scripts and tests can capture the commands alone.
        for test in &prepared.tests {
            println!("{}", test.invocation.command_line());
        }
        return Ok(());
    }
    if let Some(summary) = &prepared.build {
        shell.finished(
            &ctx.resolved.profile,
            Counts {
                noun: "built",
                built: summary.built,
                cached: summary.cached,
            },
        );
    }
    if prepared.tests.is_empty() {
        shell.note("no integration tests to run");
        return Ok(());
    }

    let total = prepared.tests.len();
    let mut failed = 0;
    for test in prepared.tests {
        let name = test.name.clone();
        shell.status("Running", format_args!("test {name}"));
        let status = gluon_core::run::run_test(test, prepared.success_exit_code)?;
        let result = match status {
            TestStatus::Passed => "ok".to_string(),
            TestStatus::Failed { code: Some(code) } => format!("FAILED (QEMU exited with {code})"),
            TestStatus::Failed { code: None } => "FAILED (QEMU was killed)".to_string(),
            TestStatus::TimedOut => "FAILED (timed out)".to_string(),
        };
        if status != TestStatus::Passed {
            failed += 1;
        }
        eprintln!("test {name} ... {result}");
    }
    let passed = total - failed;
    if failed > 0 {
        bail!("{failed} of {total} tests failed, {passed} passed");
    }
    eprintln!("test result: ok. {passed} passed");
    Ok(())
}
