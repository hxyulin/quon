//! `quon vendor` subcommand: a thin CLI wrapper over
//! [`quon_core::manifest::sources::vendor`] for the Cargo workspace,
//! and over [`quon_core::vendor::vendor_sync`] and
//! [`quon_core::vendor::vendor_check`] for `Dependency`
//! declarations.
//!
//! This file is responsible for:
//!
//! - loading `quon.rs` without expanding the Cargo workspace, because
//!   a stale vendor directory, the thing this command fixes, fails the
//!   full load;
//! - routing `--check` / `--force` / `--offline` into the right core
//!   entry point,
//! - printing human-friendly progress and summary lines,
//! - translating a non-clean [`quon_core::vendor::VendorCheckReport`]
//!   into a non-zero process exit code via
//!   [`std::process::exit`].
//!
//! The `Dependency` pipeline uses the lighter-weight
//! [`super::LayoutContext`] (no rustc probe) with
//! [`super::LayoutContextOpts::skip_vendor_autoreg`] set, because
//! registering against a stale `quon.lock` would abort before this
//! command could repair it.

use super::{LayoutContext, LayoutContextOpts, build_layout_context_at_with_opts};
use crate::cli::VendorArgs;
use anyhow::{Context, Result};
use quon_core::manifest::sources;
use quon_core::model::RawCargoWorkspace;
use quon_core::vendor::{self, VendorOptions};
use quon_core::{find_project_root, load_raw_build_model};
use std::path::Path;

/// Entry point called from `main.rs`.
pub fn run(
    cwd: &Path,
    profile: Option<&str>,
    target: Option<&str>,
    config_file: Option<&Path>,
    args: &VendorArgs,
) -> Result<()> {
    let project_root = find_project_root(cwd)
        .context("could not find a quon.rs in the current directory or any parent")?;
    let raw = load_raw_build_model(&project_root).context("failed to load build model")?;
    if let Some(ws) = &raw.cargo_workspace {
        vendor_workspace(&project_root, ws, args)?;
        if raw.external_deps.is_empty() {
            return Ok(());
        }
    }
    let ctx = build_layout_context_at_with_opts(
        cwd,
        profile,
        target,
        config_file,
        LayoutContextOpts {
            skip_vendor_autoreg: true,
        },
    )?;
    run_dependencies(ctx, args.check, args.force, args.offline)
}

/// Vendor the registry packages of the Cargo workspace `ws`, or with
/// `--check` verify the vendor stamp.
fn vendor_workspace(project_root: &Path, ws: &RawCargoWorkspace, args: &VendorArgs) -> Result<()> {
    let ws_root = project_root.join(&ws.dir);
    let dir = ws.vendored.as_deref().unwrap_or("vendor");
    let vendor_dir = project_root.join(dir);
    if args.check {
        sources::check_vendor_stamp(&ws_root, &vendor_dir)?;
        println!("{dir}/ matches Cargo.lock");
        return Ok(());
    }
    let n = sources::vendor(&ws_root, &vendor_dir, args.offline)?;
    println!("Vendored {n} packages into {dir}/.");
    if ws.vendored.is_none() {
        println!(
            "To build from {dir}/, add this to quon.rs:\n\n    \
             q.cargo_workspace({:?}).vendored({dir:?});",
            ws.dir
        );
    }
    Ok(())
}

/// `quon vendor` for `Dependency` declarations.
///
/// `check_mode` mirrors `quon vendor --check`: verify-only, no
/// disk mutation. `force` bypasses the fingerprint fast path.
/// `offline` passes `--offline`/`--frozen` through to cargo.
fn run_dependencies(
    ctx: LayoutContext,
    check_mode: bool,
    force: bool,
    offline: bool,
) -> Result<()> {
    let LayoutContext {
        model,
        resolved: _, // vendor doesn't need the resolved profile
        layout,
        project_root,
        ..
    } = ctx;

    if check_mode {
        let report = vendor::vendor_check(&model, &layout, &project_root)?;
        if report.is_clean() {
            println!("vendor state is up to date");
            return Ok(());
        }
        // Non-clean: print every diagnostic and exit 1. We use
        // `process::exit` rather than returning an error so the CLI
        // doesn't double-print via the top-level `Error:` prefix.
        for diag in report.to_diagnostics() {
            eprintln!("{diag}");
        }
        std::process::exit(1);
    }

    let opts = VendorOptions { force, offline };
    let lock = vendor::vendor_sync(&model, &layout, &project_root, opts)?;
    if lock.packages.is_empty() {
        println!("no external dependencies declared — wrote empty quon.lock");
    } else {
        println!(
            "vendored {} crate(s); see {}",
            lock.packages.len(),
            layout.vendor_dir(&project_root).display()
        );
    }
    // Cross-target deps live in per-target scratch workspaces (one
    // per cross target) so versions can diverge across targets without
    // colliding with the host pipeline's project-root vendor/. Always
    // run after the host pipeline so users see a single `quon vendor`
    // do the right thing regardless of whether they have cross deps.
    // For host-only models this short-circuits to a no-op with no
    // cargo invocation.
    vendor::cross::populate_cross_vendor_workspaces(&model, &layout, opts)?;
    Ok(())
}
