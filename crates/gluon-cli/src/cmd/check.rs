//! `gluon check` subcommand — runs the compile pipeline with the
//! check driver (`--emit=metadata` only, no codegen).
//!
//! The caller (in `main.rs`) constructs the `CmdContext` via
//! `build_context_for_driver(..., DriverKind::Check)` so the layout's
//! user-crate output dirs land under `tool/check/` instead of
//! clobbering `gluon build` artifacts. The `Finished` line counts
//! "checked" crates so users can tell which command just ran.

use super::CmdContext;
use crate::output::Shell;
use anyhow::Result;

/// Execute the metadata-only check pipeline against the prepared
/// context. The context's layout must have been built with
/// `DriverKind::Check`.
pub fn run(ctx: CmdContext, jobs: Option<usize>, shell: &mut Shell) -> Result<()> {
    debug_assert_eq!(ctx.ctx.driver(), gluon_core::DriverKind::Check);
    super::build::run_pipeline(&ctx, jobs, shell, "checked")?;
    Ok(())
}
