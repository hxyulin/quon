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
//! helper reads its inputs' slots through a [`results::ResultView`]. See
//! `docs/src/architecture/concurrency.md`.

pub mod dag;
mod results;
pub mod worker;

pub use dag::{Dag, DagNode, NodeId, build_dag};
pub use worker::{BuildSummary, JobDispatcher, WorkerPool};

use crate::compile::vendor_command::{VendorCompileInput, build_vendor_rustc_command};
use crate::compile::{CompileCrateInput, CompileCtx, PostCompileStep, compile_crate};
use crate::error::{Diagnostic, Error, Result};
use crate::image::{ImageCtx, build_image};
use crate::rule::{RuleCtx, RuleRegistry};
use crate::vendor::cross::context::{VendorBuildContext, VendorBuildId};
use dag::Dag as DagType;
use gluon_model::{
    BuildModel, BuildStepDef, CrateDef, EspDef, Handle, ImageDef, ResolvedConfig, ResolvedCrateRef,
    RuleDef, TargetDef, TargetSpec,
};
use results::{NodeResult, ResultView};
use std::collections::{BTreeMap, BTreeSet};
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
    /// Vendored-crate side-table populated by `vendor_sync` and read
    /// by the `DagNode::VendorCrate` dispatch arm. Empty when no cross
    /// vendored deps are declared, in which case no `VendorCrate` nodes
    /// are emitted by `build_dag` and the dispatch arm is unreachable.
    vendor_ctx: &'a VendorBuildContext,
    dag: &'a DagType,
    /// `resolved.crates` keyed by handle, so a crate dispatch finds its
    /// `ResolvedCrateRef` without scanning the list.
    crate_refs: BTreeMap<Handle<CrateDef>, &'a ResolvedCrateRef>,
    /// One slot per `NodeId`, written by that node when it succeeds.
    results: &'a [OnceLock<NodeResult>],
}

impl PipelineDispatcher<'_> {
    fn view(&self) -> ResultView<'_> {
        ResultView::new(self.dag, self.results)
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

fn dispatch_crate(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    view: &ResultView<'_>,
    crate_ref: &ResolvedCrateRef,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let crate_handle = crate_ref.handle;
    // For cross crates, look up the pre-built sysroot. The DAG
    // ensures the Sysroot node for this target completed before this
    // Crate node was dispatched. Crates whose group sets
    // `no_sysroot=true` skip this — they have no Sysroot DAG edge,
    // no sysroot was built for their target, and `compile_crate` is
    // configured to omit `--sysroot` and the implicit core/alloc/
    // compiler_builtins externs.
    let group_no_sysroot = model
        .crates
        .get(crate_handle)
        .and_then(|c| model.groups.get(c.group))
        .map(|g| g.no_sysroot)
        .unwrap_or(false);
    let sysroot_dir: Option<&Path> = if crate_ref.host || group_no_sysroot {
        None
    } else {
        Some(view.sysroot(crate_ref.target).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: Crate({:?}) ran before its Sysroot({:?}) dependency \
                 completed; DAG edge missing?",
                crate_handle, crate_ref.target,
            ))
        })?)
    };

    let (out_path, was_cached) = compile_crate(
        ctx,
        CompileCrateInput {
            model,
            resolved,
            crate_ref,
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

/// Cache key for one vendored-crate build.
///
/// Includes `version` because the on-disk out_dir is keyed only on
/// `cargo_target_name` (see [`crate::compile::layout::BuildLayout::host_vendor_dep_dir`]).
/// Without version in the key, a `cargo update` that bumps a vendored
/// crate's version would map to the same cache slot as the previous
/// version's build — the `argv_hash` check would catch it (the vendor
/// dir name changes), but it's clearer to encode the version directly.
fn build_vendor_cache_key(
    model: &BuildModel,
    resolved: &ResolvedConfig,
    build: &crate::vendor::cross::graph::VendorCrateBuild,
    layout: &crate::compile::layout::BuildLayout,
) -> Result<String> {
    let suffix = layout.cache_key_suffix();
    let name = &build.crate_id.name;
    let version = &build.crate_id.version;
    match build.target {
        TargetSpec::Host => Ok(format!("vendor:host:{name}:{version}{suffix}")),
        TargetSpec::Cross(target_handle) => {
            let target = model.targets.get(target_handle).ok_or_else(|| {
                Error::Compile(format!(
                    "scheduler: vendor build {name} references target handle {:?} \
                     not in build model",
                    target_handle,
                ))
            })?;
            Ok(format!(
                "vendor:cross:{name}:{version}:{}:{}{suffix}",
                resolved.profile.name, target.name,
            ))
        }
    }
}

/// Directories of every artifact transitively reachable from `build`'s
/// deps, for `-L dependency=<dir>`.
///
/// rustc loads each crate recorded in a direct dep's metadata by name,
/// so a consumer of `syn` needs `unicode-ident`'s directory even though
/// only `syn` is passed with `--extern`. Each vendor unit has its own
/// output directory, so the walk collects one directory per reached
/// unit. The walk does not descend into a proc macro's deps: those are
/// linked into the proc-macro dylib, and they are host units, whose
/// directories must not appear on a cross unit's search path.
///
/// A reached unit without an artifact is a scheduler bug: the DAG makes
/// every unit wait for its deps, and so for all transitive deps.
fn collect_vendor_search_dirs<'p>(
    vendor_ctx: &VendorBuildContext,
    vendor_artifact: impl Fn(VendorBuildId) -> Option<&'p Path>,
    build: &crate::vendor::cross::graph::VendorCrateBuild,
) -> Result<Vec<PathBuf>> {
    use crate::vendor::cross::metadata::CrateKind;
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    let mut seen: BTreeSet<VendorBuildId> = BTreeSet::new();
    let mut queue: Vec<&crate::vendor::cross::graph::VendorDepEdge> = build.deps.iter().collect();
    while let Some(edge) = queue.pop() {
        let Some(id) = vendor_ctx.lookup(edge.target, &edge.extern_name) else {
            return Err(Error::Compile(format!(
                "scheduler: vendor build {} dep edge `{}` did not resolve in vendor_ctx \
                 (lowering bug)",
                build.crate_id, edge.extern_name,
            )));
        };
        if !seen.insert(id) {
            continue;
        }
        let path = vendor_artifact(id).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: vendor build {} ran before its transitive dep {} completed; \
                 DAG edge missing?",
                build.crate_id, edge.crate_id,
            ))
        })?;
        if let Some(parent) = path.parent() {
            dirs.insert(parent.to_path_buf());
        }
        if let Some(dep) = vendor_ctx.get(id)
            && dep.kind == CrateKind::Lib
        {
            queue.extend(dep.deps.iter());
        }
    }
    Ok(dirs.into_iter().collect())
}

/// Compile one vendored crate via gluon-driven rustc.
///
/// Mirrors [`dispatch_crate`]'s shape: looks up the build descriptor,
/// resolves dep `--extern` paths from the result view, derives target/sysroot
/// for cross builds (host builds skip both), then delegates to
/// [`run_compile_and_cache`] for the freshness → spawn → mark_built dance.
///
/// Resolution failures (missing dep artifact, missing sysroot, missing
/// target in model) are scheduler invariant violations — the DAG
/// guarantees deps are built before parents and that every cross
/// vendor build has a Sysroot edge. They're surfaced as `Error::Compile`
/// rather than `unreachable!()` so a real bug fails diagnostically.
fn dispatch_vendor_crate(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    vendor_ctx: &VendorBuildContext,
    view: &ResultView<'_>,
    id: VendorBuildId,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let build = vendor_ctx.get(id).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: VendorCrate({id}) not found in vendor context (scheduler bug)",
        ))
    })?;

    // --- Resolve dep --extern paths from the deps' result slots ---
    // The DAG wires every dep edge into a vendor → vendor scheduler edge,
    // so each dep has finished by the time this arm runs. A miss is a scheduler bug, not a user mistake.
    let mut extern_artifacts: BTreeMap<String, PathBuf> = BTreeMap::new();
    for edge in &build.deps {
        let dep_id = vendor_ctx
            .lookup(edge.target, &edge.extern_name)
            .ok_or_else(|| {
                Error::Compile(format!(
                    "scheduler: vendor build {id} ({}) dep edge `{}` did not resolve in \
                 vendor_ctx (lowering bug — DAG was built but lookup now fails)",
                    build.crate_id.name, edge.extern_name,
                ))
            })?;
        let dep_path = view.vendor(dep_id).map(Path::to_path_buf).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: vendor build {id} ({}) ran before its dep {dep_id} ({}) \
                 completed; DAG edge missing?",
                build.crate_id.name, edge.crate_id.name,
            ))
        })?;
        extern_artifacts.insert(edge.extern_name.clone(), dep_path);
    }

    // --- Derive target/sysroot/out_dir based on host vs cross ---
    // Host: no --target, no --sysroot, out_dir under host/vendor-deps/.
    // Cross: triple from TargetDef::spec, sysroot from the result view, out_dir
    // under cross/<target>/<profile>/vendor-deps/.
    let (target_triple, target_triple_builtin, sysroot_owned, out_dir) = match build.target {
        TargetSpec::Host => (
            None,
            false,
            None::<PathBuf>,
            ctx.layout.host_vendor_dep_dir(&build.cargo_target_name),
        ),
        TargetSpec::Cross(target_handle) => {
            let target = model.targets.get(target_handle).ok_or_else(|| {
                Error::Compile(format!(
                    "scheduler: vendor build {id} ({}) references target handle {:?} \
                         not in build model",
                    build.crate_id.name, target_handle,
                ))
            })?;
            let sysroot = view
                .sysroot(target_handle)
                .map(Path::to_path_buf)
                .ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: vendor build {id} ({}) ran before its Sysroot({:?}) \
                         dependency completed; DAG edge missing?",
                        build.crate_id.name, target_handle,
                    ))
                })?;
            let out_dir = ctx.layout.cross_vendor_dep_dir(
                target,
                &resolved.profile,
                &build.cargo_target_name,
            );
            (
                Some(target.spec.clone()),
                target.builtin,
                Some(sysroot),
                out_dir,
            )
        }
    };

    // --- Assemble rustc command ---
    let search_dirs = collect_vendor_search_dirs(vendor_ctx, |id| view.vendor(id), build)?;
    let input = VendorCompileInput {
        build,
        extern_artifacts: &extern_artifacts,
        search_dirs: &search_dirs,
        sysroot_dir: sysroot_owned.as_deref(),
        out_dir: &out_dir,
        rustc: &ctx.rustc_info.rustc_path,
        target_triple: target_triple.as_deref(),
        target_triple_builtin,
        opt_level: resolved.profile.opt_level,
        debug_info: resolved.profile.debug_info,
    };
    let (builder, output_path, depfile_path) = build_vendor_rustc_command(&input).map_err(|e| {
        Error::Compile(format!(
            "scheduler: vendor build {id} ({}) command assembly failed: {e}",
            build.crate_id.name,
        ))
    })?;
    let cache_key = build_vendor_cache_key(model, resolved, build, &ctx.layout)?;

    // --- Cache + spawn ---
    // Capture per-build state into owned values so the error closures
    // (which outlive `build`) can reference it.
    let crate_name_fail = build.crate_id.name.clone();
    let crate_name_depfile = build.crate_id.name.clone();
    let target_label = match build.target {
        TargetSpec::Host => "host".to_string(),
        TargetSpec::Cross(h) => model
            .targets
            .get(h)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "<unknown>".to_string()),
    };
    let depfile_for_diag = depfile_path.clone();

    let (out_path, was_cached) = crate::compile::run_compile_and_cache(
        ctx,
        builder,
        cache_key,
        output_path,
        depfile_path,
        Some(&out_dir),
        Vec::new(),              // dep and sysroot rlibs come from the depfile
        Vec::new(),              // aux_outputs
        None::<PostCompileStep>, // post_compile_step
        Box::new(move |output, rendered| {
            Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "rustc failed while building vendored crate '{}' for {}: exit={:?}",
                    crate_name_fail,
                    target_label,
                    output.status.code(),
                ))
                .with_note(format!(
                    "stderr:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                ))
                .with_note(format!("command: {rendered}")),
            ])
        }),
        Box::new(move |e| {
            Error::Diagnostics(vec![
                Diagnostic::error(format!(
                    "built vendored crate '{}' but its depfile at {} could not be read",
                    crate_name_depfile,
                    depfile_for_diag.display(),
                ))
                .with_note(format!("underlying error: {e}"))
                .with_note(
                    "the artifact is on disk but the cache was not updated; \
                     the next run will re-spawn rustc for this crate",
                ),
            ])
        }),
        stderr,
    )?;

    Ok((Some(out_path), was_cached))
}

/// Human-readable name of a node for failure messages, such as
/// `crate kernel (x86_64-unknown-none, dev)` or `sysroot x86_64-unknown-none`.
fn node_label(
    node: DagNode,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    vendor_ctx: &VendorBuildContext,
) -> String {
    let target_name = |h: Handle<TargetDef>| {
        model
            .targets
            .get(h)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| format!("{h:?}"))
    };
    let platform = |spec: TargetSpec| match spec {
        TargetSpec::Host => "host".to_string(),
        TargetSpec::Cross(t) => format!("{}, {}", target_name(t), resolved.profile.name),
    };
    match node {
        DagNode::Sysroot(t) => format!("sysroot {}", target_name(t)),
        DagNode::ConfigCrate(t) => format!("config crate ({})", target_name(t)),
        DagNode::Crate(h) => match model.crates.get(h) {
            Some(c) => format!("crate {} ({})", c.name, platform(c.target)),
            None => format!("crate {h:?}"),
        },
        DagNode::VendorCrate(id) => match vendor_ctx.get(id) {
            Some(b) => format!(
                "vendored crate {} {} ({})",
                b.crate_id.name,
                b.crate_id.version,
                platform(b.target)
            ),
            None => format!("vendored crate {id}"),
        },
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
            DagNode::Crate(handle) => {
                let crate_ref = self.crate_refs.get(&handle).ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: Crate node references handle {handle:?} not in \
                         resolved.crates"
                    ))
                })?;
                // rustc writes everything to stderr; the worker pool
                // flushes it per job.
                dispatch_crate(ctx, model, resolved, &view, crate_ref, stderr)?
            }
            DagNode::Esp(handle) => dispatch_esp(ctx, model, resolved, &view, handle, stdout)?,
            DagNode::Image(handle) => dispatch_image(ctx, model, resolved, &view, handle, stdout)?,
            DagNode::Rule(handle) => dispatch_rule(
                ctx, model, resolved, self.rules, &view, handle, stdout, stderr,
            )?,
            DagNode::BuildStep(handle) => {
                dispatch_build_step(ctx, model, resolved, &view, handle, stdout, stderr)?
            }
            DagNode::VendorCrate(id) => {
                dispatch_vendor_crate(ctx, model, resolved, self.vendor_ctx, &view, id, stderr)?
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
            self.vendor_ctx,
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
    let dag = build_dag(resolved, model, vendor_ctx)?;
    let results: Vec<OnceLock<NodeResult>> = (0..dag.len()).map(|_| OnceLock::new()).collect();

    let dispatcher = PipelineDispatcher {
        ctx,
        model,
        resolved,
        rules,
        vendor_ctx,
        dag: &dag,
        crate_refs: resolved.crates.iter().map(|r| (r.handle, r)).collect(),
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
    // dispatch_vendor_crate — pre-spawn error-path coverage. These
    // exercise the resolution logic in isolation; they all fail before
    // rustc is spawned, so they don't need a real toolchain.
    // -------------------------------------------------------------------

    fn vendor_lib(
        name: &str,
        ver: &str,
        target: TargetSpec,
        deps: Vec<crate::vendor::cross::graph::VendorDepEdge>,
    ) -> crate::vendor::cross::graph::VendorCrateBuild {
        use crate::vendor::cross::graph::VendorCrateBuild;
        use crate::vendor::cross::metadata::{CrateId, CrateKind};
        VendorCrateBuild {
            crate_id: CrateId {
                name: name.into(),
                version: ver.into(),
            },
            cargo_target_name: name.replace('-', "_"),
            target,
            kind: CrateKind::Lib,
            manifest_dir: PathBuf::from(format!("/vendor/{name}-{ver}")),
            lib_path: PathBuf::from(format!("/vendor/{name}-{ver}/src/lib.rs")),
            edition: "2021".into(),
            features: vec![],
            deps,
        }
    }

    #[test]
    fn vendor_search_dirs_cover_transitive_deps_but_not_proc_macro_internals() {
        use crate::vendor::cross::graph::VendorDepEdge;
        use crate::vendor::cross::metadata::{CrateId, CrateKind};
        let cross = TargetSpec::Cross(Handle::<gluon_model::TargetDef>::new(0));
        let edge = |name: &str, target| VendorDepEdge {
            extern_name: name.into(),
            crate_id: CrateId {
                name: name.into(),
                version: "1.0.0".into(),
            },
            target,
        };
        // kernel → a → b (cross libs); kernel → derive (host proc
        // macro) → quote (host lib).
        let mut vctx = VendorBuildContext::new();
        let mut artifacts = BTreeMap::new();
        let mut add = |b: crate::vendor::cross::graph::VendorCrateBuild, dir: &str| {
            let id = vctx.add(b).unwrap();
            artifacts.insert(id, PathBuf::from(format!("/{dir}/artifact")));
        };
        add(vendor_lib("b", "1.0.0", cross, vec![]), "cross-b");
        add(
            vendor_lib("a", "1.0.0", cross, vec![edge("b", cross)]),
            "cross-a",
        );
        add(
            vendor_lib("quote", "1.0.0", TargetSpec::Host, vec![]),
            "host-quote",
        );
        let mut derive = vendor_lib(
            "derive",
            "1.0.0",
            TargetSpec::Host,
            vec![edge("quote", TargetSpec::Host)],
        );
        derive.kind = CrateKind::ProcMacro;
        add(derive, "host-derive");
        let kernel = vendor_lib(
            "kernel",
            "1.0.0",
            cross,
            vec![edge("a", cross), edge("derive", TargetSpec::Host)],
        );

        let dirs = collect_vendor_search_dirs(
            &vctx,
            |id| artifacts.get(&id).map(PathBuf::as_path),
            &kernel,
        )
        .unwrap();
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/cross-a"),
                PathBuf::from("/cross-b"),
                PathBuf::from("/host-derive"),
            ],
        );
    }

    #[test]
    fn dispatch_vendor_crate_missing_sysroot_for_cross_errors() {
        // Cross vendor build whose Sysroot has not finished — this
        // is the "Sysroot ran after VendorCrate" failure mode.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx(tmp.path());
        let (model, resolved) = make_minimal_model_and_resolved();
        let target = resolved.profile.target;
        let mut vendor_ctx = VendorBuildContext::new();
        let id = vendor_ctx
            .add(vendor_lib(
                "bitflags",
                "2.11.0",
                TargetSpec::Cross(target),
                vec![],
            ))
            .expect("add ok");
        let dag = Dag::new();
        let results: Vec<OnceLock<NodeResult>> = Vec::new();
        let view = ResultView::new(&dag, &results); // empty — nothing finished
        let mut stderr = Vec::<u8>::new();
        let err =
            dispatch_vendor_crate(&ctx, &model, &resolved, &vendor_ctx, &view, id, &mut stderr)
                .expect_err("must error");
        match err {
            Error::Compile(msg) => assert!(
                msg.contains("Sysroot") && msg.contains("DAG edge missing"),
                "unexpected diagnostic: {msg}",
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn dispatch_vendor_crate_missing_dep_artifact_errors() {
        // Dep edge resolves in vendor_ctx but its artifact isn't in
        // the result slots — the "dep ran after parent" mode.
        // Use a host build to bypass the sysroot requirement and isolate
        // the dep-artifact lookup.
        use crate::vendor::cross::graph::VendorDepEdge;
        use crate::vendor::cross::metadata::CrateId;
        let tmp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx(tmp.path());
        let (model, resolved) = make_minimal_model_and_resolved();
        let mut vendor_ctx = VendorBuildContext::new();
        let _dep_id = vendor_ctx
            .add(vendor_lib("dep", "1.0.0", TargetSpec::Host, vec![]))
            .expect("dep add ok");
        let parent_id = vendor_ctx
            .add(vendor_lib(
                "parent",
                "1.0.0",
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
        let dag = Dag::new();
        let results: Vec<OnceLock<NodeResult>> = Vec::new();
        let view = ResultView::new(&dag, &results); // empty — nothing finished
        let mut stderr = Vec::<u8>::new();
        let err = dispatch_vendor_crate(
            &ctx,
            &model,
            &resolved,
            &vendor_ctx,
            &view,
            parent_id,
            &mut stderr,
        )
        .expect_err("must error");
        match err {
            Error::Compile(msg) => assert!(
                msg.contains("ran before its dep") && msg.contains("DAG edge missing"),
                "unexpected diagnostic: {msg}",
            ),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn node_label_names_crate_target_and_profile() {
        use gluon_model::{CrateDef, CrateType, GroupDef};
        let (mut model, resolved) = make_minimal_model_and_resolved();
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
        let vctx = VendorBuildContext::new();
        assert_eq!(
            node_label(DagNode::Crate(kernel), &model, &resolved, &vctx),
            "crate kernel (x86_64-unknown-none, dev)"
        );
        assert_eq!(
            node_label(DagNode::Sysroot(target), &model, &resolved, &vctx),
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
