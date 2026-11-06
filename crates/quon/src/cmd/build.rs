//! `quon build` subcommand — runs the full compile pipeline.
//!
//! [`run_pipeline`] is shared with `quon check` and `quon clippy`,
//! which differ only in the driver their context was built with and in
//! the noun on the `Finished` line.

use super::CmdContext;
use crate::output::{BuildReporter, Counts, Shell};
use anyhow::Result;
use quon_core::BuildSummary;

/// Execute the build pipeline against the prepared context.
pub fn run(ctx: CmdContext, jobs: Option<usize>, shell: &mut Shell) -> Result<()> {
    run_pipeline(&ctx, jobs, shell, "built")?;
    Ok(())
}

/// Build several run entries in one schedule, then print one
/// `Finished` line. The contexts share a project and a driver, so the
/// first one's compile context serves every entry.
pub fn run_entries(
    ctxs: &[CmdContext],
    jobs: Option<usize>,
    shell: &mut Shell,
    noun: &'static str,
) -> Result<()> {
    let [first, ..] = ctxs else {
        return Ok(());
    };
    let workers = jobs.unwrap_or_else(default_jobs);
    let entries: Vec<_> = ctxs.iter().map(|c| (&c.model, &c.resolved)).collect();
    shell.status("Building", format!("{} run entries", ctxs.len()));
    let mut reporter = BuildReporter::new(shell, first.ctx.driver());
    let summary =
        quon_core::build_entries_with_reporter(&first.ctx, &entries, workers, &mut reporter)?;
    drop(reporter);
    shell.finished(
        &first.resolved.profile,
        Counts {
            noun,
            built: summary.built,
            cached: summary.cached,
            copies: summary.crates_with_copies,
        },
    );
    Ok(())
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Run the pipeline with the context's driver, print job status lines
/// as jobs finish, and end with the `Finished` line.
///
/// With `jobs` unset, uses the host's available parallelism. The CLI
/// layer (`cli::parse_jobs`) has already rejected `-j 0`.
pub fn run_pipeline(
    ctx: &CmdContext,
    jobs: Option<usize>,
    shell: &mut Shell,
    noun: &'static str,
) -> Result<BuildSummary> {
    let workers = jobs.unwrap_or_else(default_jobs);
    let mut reporter = BuildReporter::new(shell, ctx.ctx.driver());
    let summary = quon_core::build_with_reporter(
        &ctx.ctx,
        &ctx.model,
        &ctx.resolved,
        workers,
        &mut reporter,
    )?;
    // Erases the progress line before the `Finished` line prints.
    drop(reporter);
    shell.finished(
        &ctx.resolved.profile,
        Counts {
            noun,
            built: summary.built,
            cached: summary.cached,
            copies: 0,
        },
    );
    Ok(summary)
}
