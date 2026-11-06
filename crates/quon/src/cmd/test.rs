//! `quon test` subcommand: builds the active profile with its tests,
//! then runs the host tests, the integration tests and the boot tests,
//! in that order. Host tests come first because they are the fastest
//! and need no QEMU. A failed run does not stop the others; the command
//! fails at the end if any run failed.
//!
//! See [`quon_core::run::host`] for the host tests,
//! [`quon_core::run::prepare_tests`] for the integration tests, and
//! [`quon_core::run::prepare`], which boot tests reuse with the
//! suite's profile.

use super::CmdContext;
use crate::cli::TestArgs;
use crate::output::{BuildReporter, Counts, Shell};
use anyhow::{Context, Result, bail};
use quon_core::TestStatus;
use quon_core::model::{BootMode, ResolvedConfig};
use quon_core::run::Invocation;
use std::ffi::OsString;
use std::time::Duration;

/// Which kinds of test run. With no flag set, all of them.
struct Kinds {
    host: bool,
    integration: bool,
    boot: bool,
}

impl Kinds {
    fn from_args(args: &TestArgs) -> Self {
        let all = !(args.host || args.integration || args.boot);
        Self {
            host: all || args.host,
            integration: all || args.integration,
            boot: all || args.boot,
        }
    }
}

/// The outcome of every run, in run order.
#[derive(Default)]
struct Results {
    /// (kind, name, passed)
    runs: Vec<(&'static str, String, bool)>,
}

impl Results {
    fn record(&mut self, kind: &'static str, name: String, passed: bool) {
        self.runs.push((kind, name, passed));
    }

    fn passed(&self, kind: &str) -> usize {
        self.runs
            .iter()
            .filter(|(k, _, p)| *k == kind && *p)
            .count()
    }
}

/// Execute the `quon test` pipeline against the prepared context.
pub fn run(ctx: CmdContext, jobs: Option<usize>, args: TestArgs, shell: &mut Shell) -> Result<()> {
    let kinds = Kinds::from_args(&args);
    let boot_mode_override = match (args.uefi, args.direct) {
        (true, _) => Some(BootMode::Uefi),
        (_, true) => Some(BootMode::Direct),
        _ => None,
    };
    let opts = quon_core::TestOptions {
        boot_mode_override,
        timeout_override: args.timeout.map(Duration::from_secs),
        extra_args: args.extra,
        workers: jobs,
        filter: args.filter,
        dry_run: args.dry_run,
        no_build: args.no_build,
        runners: super::runners(),
    };
    let filter = opts.filter.as_deref();

    let has_integration = kinds.integration
        && !quon_core::run::integration_tests(&ctx.model, &ctx.resolved, filter).is_empty();
    let suites: Vec<(String, String)> = if kinds.boot {
        ctx.model
            .boot_tests
            .iter()
            .filter(|(_, t)| filter.is_none_or(|f| t.name.contains(f)))
            .filter_map(|(_, t)| {
                let profile = ctx.model.profiles.get(t.profile)?;
                Some((t.name.clone(), profile.name.clone()))
            })
            .collect()
    } else {
        Vec::new()
    };
    // Declared integration tests that cannot run must fail the run, not
    // be skipped. Each boot test checks its own profile's runner.
    if has_integration {
        quon_core::run::profile_runner(&ctx.model, &ctx.resolved)?;
    }

    // One build of the active profile serves the host and integration
    // tests. Boot tests build their own profile below.
    let summary = if opts.dry_run || opts.no_build || !(kinds.host || has_integration) {
        None
    } else {
        let mut reporter = BuildReporter::new(shell, ctx.ctx.driver());
        let summary =
            quon_core::run::build_tests(&ctx.ctx, &ctx.model, &ctx.resolved, &opts, &mut reporter)?;
        // Erases the progress line before anything else prints.
        drop(reporter);
        shell.finished(
            &ctx.resolved.profile,
            Counts {
                noun: "built",
                built: summary.built,
                cached: summary.cached,
            },
        );
        Some(summary)
    };

    let mut results = Results::default();
    if kinds.host {
        run_host_tests(&ctx, &opts, shell, &mut results)?;
    }
    if has_integration {
        let tests = quon_core::run::prepare_tests(
            &ctx.ctx,
            &ctx.model,
            &ctx.resolved,
            &opts,
            summary.as_ref(),
        )?;
        for test in tests {
            run_invocation(
                "integration",
                test.name,
                test.invocation,
                &opts,
                shell,
                &mut results,
            )?;
        }
    }
    for (name, profile) in suites {
        run_boot_test(&ctx, jobs, &name, &profile, &opts, shell, &mut results)?;
    }

    if opts.dry_run {
        return Ok(());
    }
    report(&results, shell)
}

/// Run each host test executable, with the filter passed to libtest.
fn run_host_tests(
    ctx: &CmdContext,
    opts: &quon_core::TestOptions,
    shell: &mut Shell,
    results: &mut Results,
) -> Result<()> {
    let tests = quon_core::run::host_tests(&ctx.ctx, &ctx.model, &ctx.resolved)?;
    let libtest_args: Vec<OsString> = opts.filter.iter().map(OsString::from).collect();
    if !opts.dry_run && !tests.is_empty() {
        quon_core::run::host::create_tmpdir(&ctx.resolved)?;
    }
    for test in tests {
        let name = format!("{} ({})", test.package, test.source);
        let mut cmd = test.command(&libtest_args);
        if test.unbuilt {
            if opts.dry_run {
                // A comment, so a script running the lines skips it.
                println!(
                    "# {name}: rustdoc --test, with the command the build stores in {}",
                    test.program.display()
                );
                continue;
            }
            shell.error(&anyhow::anyhow!(
                "host test {name}: the build stored no doctest command in {}; \
                 run `quon test` without --no-build",
                test.program.display()
            ));
            results.record("host", name, false);
            continue;
        }
        if opts.dry_run {
            // stdout, so scripts and tests can capture the commands alone.
            println!("{}", command_line(&cmd));
            continue;
        }
        shell.status("Running", format_args!("host test {name}"));
        let status = cmd
            .status()
            .with_context(|| format!("failed to run {}", test.program.display()))?;
        results.record("host", name, status.success());
    }
    Ok(())
}

/// Build the suite's profile, boot its run binary, and record the
/// result.
fn run_boot_test(
    ctx: &CmdContext,
    jobs: Option<usize>,
    name: &str,
    profile: &str,
    opts: &quon_core::TestOptions,
    shell: &mut Shell,
    results: &mut Results,
) -> Result<()> {
    let overrides = (!ctx.overrides.is_empty()).then_some(&ctx.overrides);
    let resolved: ResolvedConfig = quon_core::resolve_config(
        &ctx.model,
        profile,
        ctx.target.as_deref(),
        &ctx.project_root,
        overrides,
    )
    .with_context(|| format!("boot test '{name}': failed to resolve profile '{profile}'"))?;
    let run_opts = quon_core::RunOptions {
        workers: jobs,
        ..opts.run_options()
    };
    let mut reporter = BuildReporter::new(shell, ctx.ctx.driver());
    let prepared =
        quon_core::run::prepare(&ctx.ctx, &ctx.model, &resolved, &run_opts, &mut reporter)?;
    drop(reporter);
    if let Some(summary) = &prepared.build {
        shell.finished(
            &resolved.profile,
            Counts {
                noun: "built",
                built: summary.built,
                cached: summary.cached,
            },
        );
    }
    run_invocation(
        "boot",
        name.to_string(),
        prepared.invocation,
        opts,
        shell,
        results,
    )
}

/// Run one integration test or boot test with its runner and record the
/// result, or print its command under `--dry-run`.
fn run_invocation(
    kind: &'static str,
    name: String,
    invocation: Invocation,
    opts: &quon_core::TestOptions,
    shell: &mut Shell,
    results: &mut Results,
) -> Result<()> {
    if opts.dry_run {
        println!("{}", invocation.command_line());
        return Ok(());
    }
    shell.status("Running", format_args!("{kind} test {name}"));
    let runner = invocation.runner.clone();
    let status = quon_core::run::run_test(invocation)?;
    let result = match status {
        TestStatus::Passed => "ok".to_string(),
        TestStatus::Failed { code: Some(code) } => {
            format!("FAILED (runner '{runner}' exited with {code})")
        }
        TestStatus::Failed { code: None } => format!("FAILED (runner '{runner}' was killed)"),
        TestStatus::TimedOut => "FAILED (timed out)".to_string(),
    };
    eprintln!("{kind} test {name} ... {result}");
    results.record(kind, name, status == TestStatus::Passed);
    Ok(())
}

/// Print one summary over every kind and fail if any run failed.
fn report(results: &Results, shell: &mut Shell) -> Result<()> {
    if results.runs.is_empty() {
        shell.note("no tests to run");
        return Ok(());
    }
    let counts: Vec<String> = ["host", "integration", "boot"]
        .into_iter()
        .filter(|k| results.runs.iter().any(|(rk, _, _)| rk == k))
        .map(|k| {
            let total = results.runs.iter().filter(|(rk, _, _)| *rk == k).count();
            format!("{k} {} of {total}", results.passed(k))
        })
        .collect();
    let failed: Vec<String> = results
        .runs
        .iter()
        .filter(|(_, _, passed)| !passed)
        .map(|(kind, name, _)| format!("{kind} test {name}"))
        .collect();
    if failed.is_empty() {
        eprintln!("test result: ok. passed: {}", counts.join(", "));
        return Ok(());
    }
    eprintln!("failures:");
    for f in &failed {
        eprintln!("    {f}");
    }
    bail!(
        "{} of {} test runs failed (passed: {})",
        failed.len(),
        results.runs.len(),
        counts.join(", ")
    )
}

/// `cmd` as one shell-like line, for `--dry-run`.
fn command_line(cmd: &std::process::Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}
