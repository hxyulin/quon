//! `gluon build` subcommand — runs the full compile pipeline.
//!
//! [`run_pipeline`] is shared with `gluon check` and `gluon clippy`,
//! which differ only in the driver their context was built with and in
//! the noun on the `Finished` line.

use super::CmdContext;
use crate::output::{BuildReporter, Counts, Shell};
use anyhow::Result;
use gluon_core::BuildSummary;

/// Execute the build pipeline against the prepared context.
pub fn run(ctx: CmdContext, jobs: Option<usize>, shell: &mut Shell) -> Result<()> {
    run_pipeline(&ctx, jobs, shell, "built")?;
    Ok(())
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
    let workers = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    let mut reporter = BuildReporter::new(shell, ctx.ctx.driver());
    let summary = gluon_core::build_with_reporter(
        &ctx.ctx,
        &ctx.model,
        &ctx.resolved,
        workers,
        &mut reporter,
    )?;
    shell.finished(
        &ctx.resolved.profile,
        Counts {
            noun,
            built: summary.built,
            cached: summary.cached,
        },
    );
    Ok(summary)
}
