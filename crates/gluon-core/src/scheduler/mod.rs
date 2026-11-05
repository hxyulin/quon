//! Scheduler: DAG of build nodes + worker pool.
//!
//! [`dag`] defines [`DagNode`], [`Dag`], and [`build_dag`] — the types and
//! constructor that translate a [`ResolvedConfig`] + [`BuildModel`] into a
//! dependency graph.
//!
//! [`worker`] defines [`JobDispatcher`] and [`WorkerPool`] — the execution
//! engine that drives a [`Dag`] to completion, either single-threaded (`-j1`)
//! or multi-threaded.
//!
//! [`execute_pipeline`] wires the above together: builds the DAG, runs the
//! worker pool, and dispatches each node to the appropriate per-variant
//! helper. Each node writes its output to its own result slot once; a
//! helper reads its inputs' slots through a `ResultView`. See
//! `docs/src/architecture/concurrency.md`.

pub mod dag;
mod results;
pub mod worker;

pub use dag::{Dag, DagNode, NodeId, build_dag};
pub use worker::{BuildSummary, JobDispatcher, WorkerPool};

use crate::compile::{CompileCtx, CompileUnitInput, compile_unit};
use crate::error::{Error, Result};
use crate::image::{ImageCtx, build_image};
use crate::rule::{RuleCtx, RuleRegistry};
use crate::unit::{Platform, UnitGraph, UnitId};
use crate::vendor::cross::context::VendorBuildContext;
use dag::Dag as DagType;
use gluon_model::{
    BuildModel, BuildStepDef, EspDef, Handle, ImageDef, ResolvedConfig, RuleDef, TargetDef,
};
use results::{NodeResult, ResultView};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// PipelineDispatcher
// ---------------------------------------------------------------------------

/// Maps each `DagNode` variant to the appropriate helper or compile call.
///
/// Implements `JobDispatcher` so it can be driven by `WorkerPool`. The
/// only shared mutable state is `results`, one `OnceLock` per node, which
/// each node writes once and its dependents read without a lock.
struct PipelineDispatcher<'a> {
    ctx: &'a CompileCtx,
    model: &'a BuildModel,
    resolved: &'a ResolvedConfig,
    rules: &'a RuleRegistry,
    /// The unit table; one `Compile` node per unit.
    units: &'a UnitGraph,
    dag: &'a DagType,
    /// `NodeId` of each unit's `Compile` node, indexed by `UnitId`.
    node_of_unit: &'a [NodeId],
    /// One slot per `NodeId`, written by that node when it succeeds.
    results: &'a [OnceLock<NodeResult>],
}

impl PipelineDispatcher<'_> {
    fn view(&self) -> ResultView<'_> {
        ResultView::new(self.dag, self.results, self.units, self.node_of_unit)
    }
}

// ---------------------------------------------------------------------------
// Per-node dispatch helpers
//
// Each `dispatch_*` reads its inputs through a `ResultView` and returns
// `(primary output, was_cached)`. The dispatch arm stores that in the
// node's result slot. Helpers never see a lock.
// ---------------------------------------------------------------------------

type NodeOutcome = (Option<PathBuf>, bool);

fn dispatch_sysroot(
    ctx: &CompileCtx,
    model: &BuildModel,
    target: Handle<TargetDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let (path, was_cached) =
        crate::sysroot::ensure_sysroot_for_node(ctx, model, target, stdout, stderr)?;
    Ok((Some(path), was_cached))
}

fn dispatch_config_crate(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    view: &ResultView<'_>,
    target: Handle<TargetDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    // The DAG wires ConfigCrate(target) → Sysroot(target) so this
    // lookup must always succeed.
    let sysroot_dir = view.sysroot(target).ok_or_else(|| {
        Error::Compile(
            "scheduler: ConfigCrate ran before its Sysroot dependency; \
             DAG edge missing?"
                .into(),
        )
    })?;
    let (_name, rlib_path, was_cached) = crate::generated::config_crate::ensure_config_crate(
        ctx,
        model,
        resolved,
        target,
        sysroot_dir,
        stdout,
        stderr,
    )?;
    Ok((Some(rlib_path), was_cached))
}

fn dispatch_compile(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    units: &UnitGraph,
    view: &ResultView<'_>,
    unit: UnitId,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    // The DAG gives a unit that links the sysroot an edge from its
    // target's Sysroot node, so the sysroot has finished by now.
    let u = units.get(unit);
    let sysroot_dir: Option<&Path> = match (u.sysroot, u.platform) {
        (true, Platform::Cross(t)) => Some(view.sysroot(t).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: {} ran before its Sysroot({t:?}) dependency completed; \
                 DAG edge missing?",
                u.name,
            ))
        })?),
        _ => None,
    };
    let (out_path, was_cached) = compile_unit(
        ctx,
        CompileUnitInput {
            model,
            resolved,
            units,
            unit,
            artifacts: view,
            sysroot_dir,
        },
        stderr,
    )?;
    Ok((Some(out_path), was_cached))
}

fn dispatch_esp(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    view: &ResultView<'_>,
    esp_handle: Handle<EspDef>,
    stdout: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let (path, was_cached) =
        crate::generated::esp::ensure_esp(ctx, model, resolved, esp_handle, view, stdout)?;
    Ok((Some(path), was_cached))
}

fn dispatch_image(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    view: &ResultView<'_>,
    image_handle: Handle<ImageDef>,
    stdout: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let ictx = ImageCtx {
        ctx,
        model,
        resolved,
        artifacts: view,
    };
    let (path, was_cached) = build_image(image_handle, &ictx, stdout)?;
    Ok((Some(path), was_cached))
}

#[allow(clippy::too_many_arguments)]
fn dispatch_rule(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    rules: &RuleRegistry,
    view: &ResultView<'_>,
    rule_handle: Handle<RuleDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let rule = model.rules.get(rule_handle).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: Rule node references handle {:?} not in model.rules",
            rule_handle
        ))
    })?;
    let rule_ctx = RuleCtx {
        layout: &ctx.layout,
        resolved,
        model,
        artifacts: view,
    };
    rules.dispatch(&rule_ctx, rule, stdout, stderr)?;
    // Rules are not cacheable; the summary does not count them.
    Ok((None, false))
}

fn dispatch_build_step(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    view: &ResultView<'_>,
    handle: Handle<BuildStepDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let (out_path, was_cached) = crate::compile::build_step::execute_build_step(
        ctx, model, resolved, handle, view, stdout, stderr,
    )?;
    Ok((Some(out_path), was_cached))
}

/// Human-readable name of a node for failure messages, such as
/// `crate kernel (x86_64-unknown-none, dev)` or `sysroot x86_64-unknown-none`.
fn node_label(
    node: DagNode,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    units: &UnitGraph,
) -> String {
    let target_name = |h: Handle<TargetDef>| {
        model
            .targets
            .get(h)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| format!("{h:?}"))
    };
    match node {
        DagNode::Sysroot(t) => format!("sysroot {}", target_name(t)),
        DagNode::ConfigCrate(t) => format!("config crate ({})", target_name(t)),
        DagNode::Compile(u) => units.label(u, model, resolved),
        DagNode::Rule(h) => match model.rules.get(h) {
            Some(r) => format!("rule {}", r.name),
            None => format!("rule {h:?}"),
        },
        DagNode::Esp(h) => match model.esps.get(h) {
            Some(e) => format!("esp {}", e.name),
            None => format!("esp {h:?}"),
        },
        DagNode::Image(h) => match model.images.get(h) {
            Some(i) => format!("image {}", i.name),
            None => format!("image {h:?}"),
        },
        DagNode::BuildStep(h) => match model.build_steps.get(h) {
            Some(s) => format!("build step {}", s.name),
            None => format!("build step {h:?}"),
        },
    }
}

impl JobDispatcher for PipelineDispatcher<'_> {
    fn dispatch(&self, node: NodeId, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> Result<()> {
        let view = self.view();
        let (ctx, model, resolved) = (self.ctx, self.model, self.resolved);
        let (primary, was_cached) = match self.dag.nodes[node as usize] {
            DagNode::Sysroot(target) => dispatch_sysroot(ctx, model, target, stdout, stderr)?,
            DagNode::ConfigCrate(target) => {
                dispatch_config_crate(ctx, model, resolved, &view, target, stdout, stderr)?
            }
            // rustc writes everything to stderr; the worker pool
            // flushes it per job.
            DagNode::Compile(unit) => {
                dispatch_compile(ctx, model, resolved, self.units, &view, unit, stderr)?
            }
            DagNode::Esp(handle) => dispatch_esp(ctx, model, resolved, &view, handle, stdout)?,
            DagNode::Image(handle) => dispatch_image(ctx, model, resolved, &view, handle, stdout)?,
            DagNode::Rule(handle) => dispatch_rule(
                ctx, model, resolved, self.rules, &view, handle, stdout, stderr,
            )?,
            DagNode::BuildStep(handle) => {
                dispatch_build_step(ctx, model, resolved, &view, handle, stdout, stderr)?
            }
        };
        self.results[node as usize]
            .set(NodeResult {
                primary,
                was_cached,
            })
            .map_err(|_| {
                Error::Compile(format!(
                    "scheduler: {} was dispatched twice (scheduler bug)",
                    self.label(node)
                ))
            })
    }

    fn label(&self, node: NodeId) -> String {
        node_label(
            self.dag.nodes[node as usize],
            self.model,
            self.resolved,
            self.units,
        )
    }
}

// ---------------------------------------------------------------------------
// execute_pipeline
// ---------------------------------------------------------------------------

/// Build the DAG, dispatch every node through the worker pool, and return
/// once all nodes have completed successfully.
///
/// `workers` is clamped to `>= 1` by `WorkerPool::new`.
///
/// ### Shared state
///
/// Each node stores its output in its own `OnceLock` slot. The DAG
/// guarantees topological order, so a node's inputs are always set
/// before the node runs and reading them needs no lock.
///
/// ### stdout / stderr
///
/// Per-job output is buffered in `Vec<u8>` inside the worker pool and
/// flushed to `stdout`/`stderr` as each job completes. This prevents
/// interleaved rustc output from parallel compilations.
#[allow(clippy::too_many_arguments)]
pub fn execute_pipeline(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    vendor_ctx: &VendorBuildContext,
    rules: &RuleRegistry,
    workers: usize,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<BuildSummary> {
    let units = crate::unit::lower(model, resolved, vendor_ctx)?;
    let dag = build_dag(resolved, model, &units)?;
    let node_of_unit = results::node_of_unit(&dag, &units);
    let results: Vec<OnceLock<NodeResult>> = (0..dag.len()).map(|_| OnceLock::new()).collect();

    let dispatcher = PipelineDispatcher {
        ctx,
        model,
        resolved,
        rules,
        units: &units,
        dag: &dag,
        node_of_unit: &node_of_unit,
        results: &results,
    };

    let pool = WorkerPool::new(workers);
    pool.execute(&dag, &dispatcher, stdout, stderr)?;

    Ok(summarize(&dag, &results))
}

/// Assemble the build summary from the result slots after the pool
/// has joined. Rule nodes are not cacheable and are not counted.
fn summarize(dag: &DagType, results: &[OnceLock<NodeResult>]) -> BuildSummary {
    let mut summary = BuildSummary::default();
    for (node, slot) in dag.nodes.iter().zip(results) {
        let Some(result) = slot.get() else { continue };
        if !matches!(node, DagNode::Rule(_)) {
            if result.was_cached {
                summary.cached += 1;
            } else {
                summary.built += 1;
            }
        }
        let Some(path) = result.primary.clone() else {
            continue;
        };
        match *node {
            DagNode::Esp(h) => {
                summary.esp_dirs.insert(h, path);
            }
            DagNode::Image(h) => {
                summary.image_paths.insert(h, path);
            }
            DagNode::BuildStep(h) => {
                summary.build_step_paths.insert(h, path);
            }
            _ => {}
        }
    }
    summary
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;
    use crate::compile::{BuildLayout, RustcInfo};
    use gluon_model::{
        BuildModel, PanicStrategy, ProjectDef, ResolvedConfig, ResolvedProfile, TargetDef,
        TargetSpec,
    };
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn fake_rustc_info(rustc_path: impl Into<PathBuf>) -> RustcInfo {
        RustcInfo {
            rustc_path: rustc_path.into(),
            rustc_arg: "rustc".into(),
            version: "rustc 0.0.0 (test 2020-01-01)".into(),
            host_triple: "x86_64-unknown-linux-gnu".into(),
            commit_hash: Some("deadbeef".into()),
            release: "0.0.0".into(),
            sysroot: PathBuf::from("/fake-sysroot"),
            rust_src: None,
            mtime_ns: 0,
        }
    }

    fn make_ctx(tmp: &std::path::Path) -> CompileCtx {
        let layout = BuildLayout::new(tmp.join("build"), "testproj");
        let info = fake_rustc_info("/usr/bin/rustc");
        std::fs::create_dir_all(tmp.join("build")).unwrap();
        let cache = Cache::load(tmp.join("build/cache-manifest.json")).0;
        CompileCtx::new(layout, Arc::new(info), cache)
    }

    fn make_minimal_model_and_resolved() -> (BuildModel, ResolvedConfig) {
        let mut model = BuildModel::default();
        let target = TargetDef {
            name: "x86_64-unknown-none".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            panic_strategy: Some(PanicStrategy::Abort),
            span: None,
        };
        let (target_handle, _) = model.targets.insert("x86_64-unknown-none".into(), target);

        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "testproj".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: target_handle,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            },
            options: BTreeMap::new(),
            crates: Vec::new(),
            build_dir: "/tmp/build".into(),
            project_root: "/tmp".into(),
        };
        (model, resolved)
    }

    // -------------------------------------------------------------------
    // dispatch_compile — pre-spawn error paths. These fail before rustc
    // is spawned, so they need no toolchain.
    // -------------------------------------------------------------------

    fn vendor_lib(
        name: &str,
        target: TargetSpec,
        deps: Vec<crate::vendor::cross::graph::VendorDepEdge>,
    ) -> crate::vendor::cross::graph::VendorCrateBuild {
        use crate::vendor::cross::graph::VendorCrateBuild;
        use crate::vendor::cross::metadata::{CrateId, CrateKind};
        VendorCrateBuild {
            crate_id: CrateId {
                name: name.into(),
                version: "1.0.0".into(),
            },
            cargo_target_name: name.replace('-', "_"),
            target,
            kind: CrateKind::Lib,
            manifest_dir: PathBuf::from(format!("/vendor/{name}")),
            lib_path: PathBuf::from(format!("/vendor/{name}/src/lib.rs")),
            edition: "2021".into(),
            features: vec![],
            deps,
        }
    }

    /// Dispatch `unit` against a view in which no node has finished.
    fn dispatch_with_nothing_finished(
        vendor_ctx: &VendorBuildContext,
        pick: impl Fn(&UnitGraph) -> UnitId,
    ) -> Error {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx(tmp.path());
        let (model, resolved) = make_minimal_model_and_resolved();
        let units = crate::unit::lower(&model, &resolved, vendor_ctx).expect("lower");
        let dag = Dag::new();
        let results: Vec<OnceLock<NodeResult>> = Vec::new();
        let view = ResultView::new(&dag, &results, &units, &[]);
        let mut stderr = Vec::<u8>::new();
        dispatch_compile(
            &ctx,
            &model,
            &resolved,
            &units,
            &view,
            pick(&units),
            &mut stderr,
        )
        .expect_err("must error")
    }

    #[test]
    fn dispatch_compile_without_finished_sysroot_errors() {
        let (_, resolved) = make_minimal_model_and_resolved();
        let mut vendor_ctx = VendorBuildContext::new();
        let id = vendor_ctx
            .add(vendor_lib(
                "bitflags",
                TargetSpec::Cross(resolved.profile.target),
                vec![],
            ))
            .expect("add ok");
        match dispatch_with_nothing_finished(&vendor_ctx, |u| u.unit_of_vendor(id).unwrap()) {
            Error::Compile(msg) => assert!(
                msg.contains("Sysroot") && msg.contains("DAG edge missing"),
                "unexpected diagnostic: {msg}",
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn dispatch_compile_without_finished_dep_errors() {
        use crate::vendor::cross::graph::VendorDepEdge;
        use crate::vendor::cross::metadata::CrateId;
        let mut vendor_ctx = VendorBuildContext::new();
        vendor_ctx
            .add(vendor_lib("dep", TargetSpec::Host, vec![]))
            .expect("dep add ok");
        let parent = vendor_ctx
            .add(vendor_lib(
                "parent",
                TargetSpec::Host,
                vec![VendorDepEdge {
                    extern_name: "dep".into(),
                    crate_id: CrateId {
                        name: "dep".into(),
                        version: "1.0.0".into(),
                    },
                    target: TargetSpec::Host,
                }],
            ))
            .expect("parent add ok");
        match dispatch_with_nothing_finished(&vendor_ctx, |u| u.unit_of_vendor(parent).unwrap()) {
            Error::Compile(msg) => assert!(
                msg.contains("parent depends on dep") && msg.contains("scheduler bug"),
                "unexpected diagnostic: {msg}",
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn node_label_names_crate_target_and_profile() {
        use gluon_model::{CrateDef, CrateType, GroupDef};
        let (mut model, mut resolved) = make_minimal_model_and_resolved();
        let target = resolved.profile.target;
        let (group, _) = model.groups.insert(
            "kernel".into(),
            GroupDef {
                name: "kernel".into(),
                target: TargetSpec::Cross(target),
                ..Default::default()
            },
        );
        let (kernel, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(target),
                deps: BTreeMap::new(),
                dev_deps: BTreeMap::new(),
                features: Vec::new(),
                root: None,
                linker_script: None,
                group,
                is_project_crate: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                requires_config: Vec::new(),
                artifact_deps: Vec::new(),
                artifact_env: BTreeMap::new(),
                span: None,
            },
        );
        resolved.crates.push(gluon_model::ResolvedCrateRef {
            handle: kernel,
            target,
            host: false,
        });
        let mut vendor_ctx = VendorBuildContext::new();
        let bitflags = vendor_ctx
            .add(vendor_lib("bitflags", TargetSpec::Host, vec![]))
            .unwrap();
        let units = crate::unit::lower(&model, &resolved, &vendor_ctx).unwrap();
        let label = |node| node_label(node, &model, &resolved, &units);
        assert_eq!(
            label(DagNode::Compile(units.unit_of(kernel).unwrap())),
            "crate kernel (x86_64-unknown-none, dev)"
        );
        assert_eq!(
            label(DagNode::Compile(units.unit_of_vendor(bitflags).unwrap())),
            "vendored crate bitflags 1.0.0 (host)"
        );
        assert_eq!(
            label(DagNode::Sysroot(target)),
            "sysroot x86_64-unknown-none"
        );
    }

    /// Smoke test: an empty crate list still produces a valid DAG (Sysroot +
    /// ConfigCrate nodes), and `execute_pipeline` should fail because it tries
    /// to build a real sysroot (no rust-src) but the DAG itself is valid.
    ///
    /// We verify the error is a *compile/diagnostic* error, not a panic or
    /// scheduler invariant violation.
    #[test]
    fn execute_pipeline_with_empty_crate_list_errors_on_sysroot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx(tmp.path());
        let (model, resolved) = make_minimal_model_and_resolved();
        let rules = RuleRegistry::with_builtins();

        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();

        // Should fail because rust-src is not present (fake rustc info has None).
        let result = execute_pipeline(
            &ctx,
            &model,
            &resolved,
            &VendorBuildContext::new(),
            &rules,
            1,
            &mut stdout,
            &mut stderr,
        );

        // The pipeline must propagate an error — not panic.
        assert!(
            result.is_err(),
            "expected pipeline to fail without rust-src"
        );
        // The error must be a Diagnostics or Compile variant, not an internal panic.
        match result.unwrap_err() {
            Error::Diagnostics(_) | Error::Compile(_) => {} // expected
            other => panic!("unexpected error kind: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // End-to-end ignored tests — require `rust-src` component and real rustc.
    // Run with: cargo test -p gluon-core -- --ignored execute_pipeline
    // -----------------------------------------------------------------------

    /// Build a full pipeline with a single cross Bin crate, workers=1.
    #[test]
    #[ignore]
    fn execute_pipeline_hand_built_model_j1() {
        run_e2e_pipeline(1);
    }

    /// Same fixture, workers=4. Exercises the multi-threaded path.
    #[test]
    #[ignore]
    fn execute_pipeline_hand_built_model_multi_worker() {
        run_e2e_pipeline(4);
    }

    /// Two consecutive runs — the second must hit the cache and complete
    /// in under 2 seconds (loose bound to tolerate CI variance).
    #[test]
    #[ignore]
    fn execute_pipeline_cache_hit_on_second_run() {
        use std::time::Instant;

        let info = match RustcInfo::probe() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("e2e test skipped: rustc probe failed: {e}");
                return;
            }
        };
        if info.rust_src.is_none() {
            eprintln!("e2e test skipped: rust-src component not installed");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let (ctx, model, resolved) = make_e2e_fixture(&info, tmp.path());

        let rules = RuleRegistry::with_builtins();

        // First run — cold build.
        {
            let mut out = Vec::<u8>::new();
            let mut err = Vec::<u8>::new();
            execute_pipeline(
                &ctx,
                &model,
                &resolved,
                &VendorBuildContext::new(),
                &rules,
                1,
                &mut out,
                &mut err,
            )
            .expect("first run should succeed");
        }
        ctx.cache
            .lock()
            .expect("cache mutex poisoned")
            .save()
            .expect("save cache");

        // Second run — should hit cache for everything.
        let start = Instant::now();
        {
            let mut out = Vec::<u8>::new();
            let mut err = Vec::<u8>::new();
            execute_pipeline(
                &ctx,
                &model,
                &resolved,
                &VendorBuildContext::new(),
                &rules,
                1,
                &mut out,
                &mut err,
            )
            .expect("second run should succeed");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 2,
            "second run took too long ({elapsed:?}); cache hit expected"
        );
    }

    /// Stage 8 acceptance: two consecutive `-j1` runs produce
    /// byte-identical artifact paths and binary contents. The second
    /// run is a cache hit; this test asserts not just that it
    /// succeeds (covered by `execute_pipeline_cache_hit_on_second_run`)
    /// but that nothing about the *output set* changes between runs —
    /// the result-slot dispatch must be deterministic.
    #[test]
    #[ignore]
    fn execute_pipeline_j1_is_deterministic_across_runs() {
        let info = match RustcInfo::probe() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("e2e test skipped: rustc probe failed: {e}");
                return;
            }
        };
        if info.rust_src.is_none() {
            eprintln!("e2e test skipped: rust-src component not installed");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let (ctx, model, resolved) = make_e2e_fixture(&info, tmp.path());
        let rules = RuleRegistry::with_builtins();

        let mut out1 = Vec::<u8>::new();
        let mut err1 = Vec::<u8>::new();
        let s1 = execute_pipeline(
            &ctx,
            &model,
            &resolved,
            &VendorBuildContext::new(),
            &rules,
            1,
            &mut out1,
            &mut err1,
        )
        .expect("first run");

        let target = model.targets.get(resolved.profile.target).expect("target");
        let bin = ctx
            .layout
            .cross_final_dir(target, &resolved.profile)
            .join("hello");
        let bin1 = std::fs::read(&bin).expect("read first-run binary");

        // Second run — same fixture, same scheduler; cache should hit.
        let mut out2 = Vec::<u8>::new();
        let mut err2 = Vec::<u8>::new();
        let s2 = execute_pipeline(
            &ctx,
            &model,
            &resolved,
            &VendorBuildContext::new(),
            &rules,
            1,
            &mut out2,
            &mut err2,
        )
        .expect("second run");
        let bin2 = std::fs::read(&bin).expect("read second-run binary");

        assert_eq!(s1.esp_dirs, s2.esp_dirs, "esp_dirs differ across runs");
        assert_eq!(
            s1.image_paths, s2.image_paths,
            "image_paths differ across runs"
        );
        assert_eq!(
            s1.build_step_paths, s2.build_step_paths,
            "build_step_paths differ across runs"
        );
        assert_eq!(bin1, bin2, "binary contents differ across runs");
    }

    // -----------------------------------------------------------------------
    // E2E fixture helpers
    // -----------------------------------------------------------------------

    fn run_e2e_pipeline(workers: usize) {
        let info = match RustcInfo::probe() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("e2e test skipped: rustc probe failed: {e}");
                return;
            }
        };
        if info.rust_src.is_none() {
            eprintln!("e2e test skipped: rust-src component not installed");
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let (ctx, model, resolved) = make_e2e_fixture(&info, tmp.path());

        let rules = RuleRegistry::with_builtins();
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();

        let _summary = execute_pipeline(
            &ctx,
            &model,
            &resolved,
            &VendorBuildContext::new(),
            &rules,
            workers,
            &mut stdout,
            &mut stderr,
        )
        .expect("pipeline should succeed");

        // Verify the binary was produced.
        let target = model
            .targets
            .get(resolved.profile.target)
            .expect("target must exist");
        let final_dir = ctx.layout.cross_final_dir(target, &resolved.profile);
        let bin = final_dir.join("hello");
        assert!(
            bin.exists(),
            "expected binary at {bin:?} to exist after pipeline"
        );
    }

    /// Build the e2e fixture: a project "testproj" with one cross Bin crate
    /// "hello" targeting x86_64-unknown-none with panic=abort.
    ///
    /// The crate source is a minimal `no_std` entry point. We use a trivial
    /// linker script so the binary links without a libc.
    fn make_e2e_fixture(
        info: &RustcInfo,
        tmp: &std::path::Path,
    ) -> (CompileCtx, BuildModel, ResolvedConfig) {
        use gluon_model::{CrateDef, CrateType, GroupDef, TargetSpec};
        use std::collections::BTreeMap;
        use std::fs;

        // --- Write crate source ---
        let hello_src_dir = tmp.join("crates/hello/src");
        fs::create_dir_all(&hello_src_dir).expect("mkdir hello/src");
        fs::write(
            hello_src_dir.join("main.rs"),
            b"#![no_std]\n\
              #![no_main]\n\
              \n\
              #[panic_handler]\n\
              fn panic(_info: &core::panic::PanicInfo) -> ! {\n\
              \tloop {}\n\
              }\n\
              \n\
              #[no_mangle]\n\
              pub extern \"C\" fn _start() -> ! {\n\
              \tloop {}\n\
              }\n",
        )
        .expect("write main.rs");

        // --- Write linker script ---
        let hello_dir = tmp.join("crates/hello");
        fs::write(
            hello_dir.join("gluon.ld"),
            b"ENTRY(_start)\n\
              SECTIONS {\n\
              \t.text : { *(.text .text*) }\n\
              \t.rodata : { *(.rodata .rodata*) }\n\
              \t.data : { *(.data .data*) }\n\
              \t.bss : { *(.bss .bss*) }\n\
              }\n",
        )
        .expect("write gluon.ld");

        // --- Build model ---
        let mut model = BuildModel::default();

        let target_def = TargetDef {
            name: "x86_64-unknown-none".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            panic_strategy: Some(PanicStrategy::Abort),
            span: None,
        };
        let (target_handle, _) = model
            .targets
            .insert("x86_64-unknown-none".into(), target_def);

        let (group_handle, _) = model.groups.insert(
            "kernel".into(),
            GroupDef {
                name: "kernel".into(),
                target: TargetSpec::Cross(target_handle),
                default_edition: "2021".into(),
                crates: vec!["hello".into()],
                shared_flags: Vec::new(),
                is_project: true,
                config: false,
                no_sysroot: false,
                span: None,
            },
        );

        let crate_def = CrateDef {
            name: "hello".into(),
            path: "crates/hello".into(),
            edition: "2021".into(),
            crate_type: CrateType::Bin,
            target: TargetSpec::Cross(target_handle),
            deps: BTreeMap::new(),
            dev_deps: BTreeMap::new(),
            features: Vec::new(),
            root: None,
            linker_script: Some("crates/hello/gluon.ld".into()),
            group: group_handle,
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
        };
        let (crate_handle, _) = model.crates.insert("hello".into(), crate_def);

        // --- Resolved config ---
        let build_dir = tmp.join("build");
        fs::create_dir_all(&build_dir).expect("mkdir build");

        let resolved = ResolvedConfig {
            project: ProjectDef {
                name: "testproj".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: ResolvedProfile {
                name: "dev".into(),
                target: target_handle,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            },
            options: BTreeMap::new(),
            crates: vec![gluon_model::ResolvedCrateRef {
                handle: crate_handle,
                target: target_handle,
                host: false,
            }],
            build_dir: build_dir.clone(),
            project_root: tmp.to_path_buf(),
        };

        // --- CompileCtx ---
        let layout = BuildLayout::new(build_dir.clone(), "testproj");
        let cache = Cache::load(layout.cache_manifest()).0;
        let ctx = CompileCtx::new(layout, Arc::new(info.clone()), cache);

        (ctx, model, resolved)
    }
}
