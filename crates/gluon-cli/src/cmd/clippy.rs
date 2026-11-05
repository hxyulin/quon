//! `gluon clippy` subcommand — runs the compile pipeline with the
//! clippy driver.
//!
//! Mirrors `cmd/check.rs` exactly except for two things:
//!
//! 1. The caller (in `main.rs`) constructs the `CmdContext` via
//!    `build_context_for_driver(..., DriverKind::Clippy)`. The layout
//!    routes user-crate output under `tool/clippy/` (so clippy
//!    artifacts cannot collide with build or check artifacts), and the
//!    rustc command builder swaps in `clippy-driver` as the program
//!    path while leaving every other flag identical.
//! 2. The `Finished` line counts "linted" crates.
//!
//! Sysroot, generated config crate, and cache manifest are shared with
//! `gluon build` and `gluon check`.

use super::CmdContext;
use crate::output::Shell;
use anyhow::Result;

/// Execute the clippy lint pipeline against the prepared context.
/// The context's layout must have been built with
/// `DriverKind::Clippy`.
pub fn run(ctx: CmdContext, jobs: Option<usize>, shell: &mut Shell) -> Result<()> {
    debug_assert_eq!(ctx.ctx.driver(), gluon_core::DriverKind::Clippy);
    super::build::run_pipeline(&ctx, jobs, shell, "linted")?;
    Ok(())
}
