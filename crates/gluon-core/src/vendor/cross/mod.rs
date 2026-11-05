//! Cross-target vendored builds.
//!
//! Whereas the host pipeline (in [`super::cargo_cmd`]) lets cargo do
//! the entire compile, the cross pipeline drives `rustc` directly so
//! the resulting `.rlib`s link against gluon's per-target sysroot
//! rather than the system `core`/`alloc`. The two pipelines share
//! `cargo vendor` for dependency fetch and `cargo metadata` for graph
//! resolution, but diverge at compile time.
//!
//! # Module layout
//!
//! - [`metadata`] — invoke `cargo metadata` against a per-target
//!   scratch workspace and parse the JSON into a target-agnostic
//!   [`metadata::ResolvedGraph`].
//! - [`graph`] — lower a `ResolvedGraph` into scheduler-node
//!   descriptors ([`graph::VendorCrateBuild`]), applying proc-macro
//!   re-targeting (proc-macros are dlopen'd by rustc and must always
//!   be host) and rejecting `build.rs`-bearing crates (MVP scope:
//!   build scripts are not yet supported).
//! - [`context`] — the side-table the scheduler reads to dispatch
//!   vendor builds. Lives outside [`gluon_model::BuildModel`] because
//!   vendor builds are synthesised at `vendor_sync` time, not declared
//!   by the user; keying it externally keeps the model faithful to the
//!   user's `gluon.rs`.
//!
//! Future modules (next slices, per the design spec):
//!
//! - scheduler integration — a `DagNode::VendorCrate(VendorBuildId)`
//!   variant plus a dispatcher that turns each `VendorCrateBuild` into
//!   a rustc invocation against gluon's per-target sysroot.

pub mod context;
pub mod graph;
pub mod metadata;

use crate::compile::layout::BuildLayout;
use crate::error::{Diagnostic, Error, Result};
use crate::vendor::cargo_cmd::{self, VendorFlags};
use crate::vendor::manifest_gen;
use gluon_model::{BuildModel, ExternalDepDef, Handle, TargetDef, TargetSpec};
use std::collections::BTreeMap;

/// Group cross-tagged external deps by their target handle. Host-tagged
/// deps are filtered out — those go through the host pipeline in
/// [`super::vendor_sync`]. Iteration order on the returned map is
/// `Handle<TargetDef>` order (insertion order in the model arena),
/// which is stable per model.
fn group_cross_deps(
    model: &BuildModel,
) -> BTreeMap<Handle<TargetDef>, Vec<Handle<ExternalDepDef>>> {
    let mut by_target: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for (dep_handle, def) in model.external_deps.iter() {
        if let TargetSpec::Cross(target) = def.target {
            by_target.entry(target).or_default().push(dep_handle);
        }
    }
    by_target
}

/// Run `cargo vendor` for each cross target's per-target scratch
/// workspace. Intended to be called by the `gluon vendor` CLI after
/// the host-pipeline [`super::vendor_sync`] has populated
/// `<project>/vendor/`.
///
/// For each target with at least one cross-tagged dep:
///
/// 1. Write the scratch workspace at
///    `<build>/vendor-workspaces/<target>/` via [`manifest_gen::write_vendor_workspace`]
///    (with a `TargetSpec::Cross(handle)` filter so only this target's
///    deps appear in the manifest).
/// 2. Invoke `cargo vendor` into `<scratch>/vendor/`.
///
/// Each cross target gets its own vendor tree (rather than sharing the
/// project-root `<project>/vendor/`) — two cross targets can legitimately
/// resolve the same crate at different versions, and a shared tree
/// would have one overwriting the other on every vendor run. Disk cost
/// is acceptable for the MVP — most embedded projects have a small
/// cross-target dep set; see the design doc's "vendor per workspace"
/// section.
///
/// Models with no cross-tagged deps return immediately without
/// spawning cargo, so host-only projects pay zero overhead.
pub fn populate_cross_vendor_workspaces(
    model: &BuildModel,
    layout: &BuildLayout,
    opts: super::VendorOptions,
) -> Result<()> {
    let by_target = group_cross_deps(model);
    if by_target.is_empty() {
        return Ok(());
    }
    let flags = VendorFlags {
        offline: opts.offline,
    };
    for target_handle in by_target.keys() {
        let target = model.targets.get(*target_handle).ok_or_else(|| {
            Error::Compile(format!(
                "vendor: external dep references target handle {target_handle:?} \
                 not present in model.targets",
            ))
        })?;
        let scratch = layout.vendor_workspace_for_target(&target.name);
        manifest_gen::write_vendor_workspace(
            &scratch,
            &model.external_deps,
            TargetSpec::Cross(*target_handle),
        )?;
        let vendor_dir = scratch.join("vendor");
        cargo_cmd::run_cargo_vendor(&scratch, &vendor_dir, flags)?;
    }
    Ok(())
}

/// Build a [`context::VendorBuildContext`] by reading the per-target
/// scratch workspaces populated by
/// [`populate_cross_vendor_workspaces`]. Intended to be called from
/// the build pipeline (replaces the empty
/// `VendorBuildContext::new()` placeholder in `build_with_workers`).
///
/// For each target with cross-tagged deps:
///
/// 1. Invoke `cargo metadata --frozen --offline` against the scratch.
///    `--offline` is hard-coded here (rather than threaded from a
///    `VendorOptions`) because by build time, all network work
///    happened during `gluon vendor`; if cargo would need the network
///    now, the user's vendor state is broken and we want a hard error
///    rather than a silent fetch.
/// 2. Parse the JSON into a [`metadata::ResolvedGraph`].
/// 3. Lower into [`graph::VendorCrateBuild`]s via
///    [`graph::build_vendor_nodes`] (proc-macro re-targeting +
///    build.rs rejection).
/// 4. Register every build with the returned context.
///
/// Models with no cross-tagged deps short-circuit and return an empty
/// context without spawning cargo, so host-only projects pay no
/// build-time overhead either.
///
/// # Errors
///
/// - The scratch workspace doesn't exist (user hasn't run
///   `gluon vendor` since adding cross deps): diagnostic includes the
///   expected path and a "run `gluon vendor`" hint.
/// - `cargo metadata` fails or produces non-parseable JSON.
/// - The graph contains a build-script crate (MVP scope rejection
///   raised by [`graph::build_vendor_nodes`]).
/// - Two cross workspaces register the same `(target, name)` —
///   typically a proc-macro pulled in transitively at different
///   versions across cross targets. Surfaced verbatim from
///   [`context::VendorBuildContext::add`].
pub fn build_cross_vendor_context(
    model: &BuildModel,
    layout: &BuildLayout,
) -> Result<context::VendorBuildContext> {
    let mut ctx = context::VendorBuildContext::new();
    let by_target = group_cross_deps(model);
    if by_target.is_empty() {
        return Ok(ctx);
    }
    let flags = VendorFlags { offline: true };
    for target_handle in by_target.keys() {
        let target = model.targets.get(*target_handle).ok_or_else(|| {
            Error::Compile(format!(
                "vendor: external dep references target handle {target_handle:?} \
                 not present in model.targets",
            ))
        })?;
        let scratch = layout.vendor_workspace_for_target(&target.name);
        if !scratch.join("Cargo.toml").exists() {
            return Err(Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "cross vendor workspace for target '{}' is missing",
                    target.name,
                ))
                .with_note(format!("expected at {}", scratch.display()))
                .with_note("run `gluon vendor` to populate it"),
            ]));
        }
        let vendor_dir = scratch.join("vendor");
        let json = cargo_cmd::run_cargo_metadata(&scratch, &vendor_dir, flags)?;
        let graph = metadata::parse_metadata(&json, *target_handle).map_err(|e| {
            Error::Diagnostics(vec![Diagnostic::error(format!(
                "failed to parse cargo metadata for target '{}': {e}",
                target.name,
            ))])
        })?;
        let builds = graph::build_vendor_nodes(&graph).map_err(|e| {
            Error::Diagnostics(vec![Diagnostic::error(format!(
                "failed to lower vendored graph for target '{}': {e}",
                target.name,
            ))])
        })?;
        for build in builds {
            ctx.add(build).map_err(|e| {
                Error::Diagnostics(vec![Diagnostic::error(format!(
                    "duplicate vendor build registration for target '{}': {e}",
                    target.name,
                ))])
            })?;
        }
    }
    Ok(ctx)
}

// -------------------- tests --------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::layout::BuildLayout;
    use crate::error::Error;
    use gluon_model::{BuildModel, DepSource, ExternalDepDef, PanicStrategy, TargetDef};

    fn empty_model() -> BuildModel {
        BuildModel::default()
    }

    fn model_with_cross_target_and_dep() -> (BuildModel, Handle<TargetDef>) {
        let mut model = BuildModel::default();
        let (target, _) = model.targets.insert(
            "kernel".into(),
            TargetDef {
                name: "kernel".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: Some(PanicStrategy::Abort),
                span: None,
            },
        );
        model.external_deps.insert(
            "bitflags".into(),
            ExternalDepDef {
                name: "bitflags".into(),
                source: DepSource::CratesIo {
                    version: "2.11.0".into(),
                },
                features: Vec::new(),
                default_features: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                target: TargetSpec::Cross(target),
                span: None,
            },
        );
        (model, target)
    }

    fn layout(tmp: &std::path::Path) -> BuildLayout {
        BuildLayout::new(tmp.join("build"), "testproj")
    }

    #[test]
    fn group_cross_deps_skips_host_targeted_entries() {
        let mut model = BuildModel::default();
        let (target, _) = model.targets.insert(
            "kernel".into(),
            TargetDef {
                name: "kernel".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        // Host-tagged dep must NOT appear in the cross grouping.
        model.external_deps.insert(
            "host-dep".into(),
            ExternalDepDef {
                name: "host-dep".into(),
                source: DepSource::CratesIo {
                    version: "1.0.0".into(),
                },
                features: Vec::new(),
                default_features: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                target: TargetSpec::Host,
                span: None,
            },
        );
        // Cross-tagged dep MUST appear.
        model.external_deps.insert(
            "cross-dep".into(),
            ExternalDepDef {
                name: "cross-dep".into(),
                source: DepSource::CratesIo {
                    version: "2.0.0".into(),
                },
                features: Vec::new(),
                default_features: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                target: TargetSpec::Cross(target),
                span: None,
            },
        );
        let grouped = group_cross_deps(&model);
        assert_eq!(grouped.len(), 1, "host dep must not appear");
        assert!(grouped.contains_key(&target));
        assert_eq!(grouped[&target].len(), 1);
    }

    #[test]
    fn build_cross_vendor_context_for_host_only_model_returns_empty_without_cargo() {
        // Host-only models must short-circuit before any cargo
        // invocation — proves the "zero overhead for projects without
        // cross deps" invariant.
        let tmp = tempfile::tempdir().expect("tempdir");
        let model = empty_model();
        let layout = layout(tmp.path());
        let ctx = build_cross_vendor_context(&model, &layout).expect("ok");
        assert!(ctx.is_empty());
        // Crucially, no scratch workspace was created.
        assert!(
            !tmp.path().join("build/vendor-workspaces").exists(),
            "no cargo work should happen for host-only models",
        );
    }

    #[test]
    fn populate_cross_vendor_workspaces_for_host_only_model_is_noop() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let model = empty_model();
        let layout = layout(tmp.path());
        populate_cross_vendor_workspaces(&model, &layout, super::super::VendorOptions::default())
            .expect("ok");
        assert!(!tmp.path().join("build/vendor-workspaces").exists());
    }

    #[test]
    fn build_cross_vendor_context_errors_with_hint_when_workspace_missing() {
        // A model declaring a cross dep without a populated scratch
        // workspace must produce a helpful diagnostic, not a generic
        // "cargo metadata failed" message.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (model, _) = model_with_cross_target_and_dep();
        let layout = layout(tmp.path());
        let err = build_cross_vendor_context(&model, &layout).expect_err("must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("missing"),
            "diagnostic should call out the missing workspace: {msg}",
        );
        assert!(
            msg.contains("gluon vendor"),
            "diagnostic should hint at the fix: {msg}",
        );
        assert!(
            msg.contains("kernel"),
            "diagnostic should name the target: {msg}",
        );
    }

    #[test]
    fn build_cross_vendor_context_errors_when_target_handle_dangling() {
        // Synthesise an external dep whose target handle index doesn't
        // exist in model.targets. This shouldn't happen in well-formed
        // models (the resolver wires handles correctly), but the
        // function must surface it as a Compile error rather than
        // panicking on the arena lookup.
        let mut model = BuildModel::default();
        // Dangling handle — never inserted.
        let dangling = Handle::<TargetDef>::new(42);
        model.external_deps.insert(
            "x".into(),
            ExternalDepDef {
                name: "x".into(),
                source: DepSource::CratesIo {
                    version: "1.0.0".into(),
                },
                features: Vec::new(),
                default_features: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                target: TargetSpec::Cross(dangling),
                span: None,
            },
        );
        let tmp = tempfile::tempdir().expect("tempdir");
        let layout = layout(tmp.path());
        let err = build_cross_vendor_context(&model, &layout).expect_err("must fail");
        match err {
            Error::Compile(msg) => assert!(
                msg.contains("not present in model.targets"),
                "unexpected diagnostic: {msg}",
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
