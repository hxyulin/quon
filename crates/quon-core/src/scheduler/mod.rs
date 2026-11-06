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
mod merged;
mod results;
pub mod worker;

pub use dag::{Dag, DagNode, NodeId, build_dag};
pub use merged::{EntryPipeline, execute_entries};
pub use results::NodeData;
pub use worker::{
    BuildSummary, JobDispatcher, JobInfo, JobKind, JobOutcome, PlainReporter, Reporter, WorkerPool,
};

use crate::compile::{CompileCtx, CompileUnitInput, compile_unit};
use crate::error::{Error, Result};
use crate::image::{ImageCtx, build_image};
use crate::rule::{RuleCtx, RuleRegistry};
use crate::unit::{Platform, RunId, UnitGraph, UnitId};
use crate::vendor::cross::context::VendorBuildContext;
use dag::Dag as DagType;
use quon_model::{
    BuildModel, BuildStepDef, EspDef, Handle, ImageDef, ResolvedConfig, RuleDef, TargetDef,
};
use results::{NodeResult, ResultView};
use std::collections::BTreeMap;
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
    /// `rustc --print cfg` of each platform that has a build-script
    /// owner, probed once before dispatch.
    script_cfgs: &'a ScriptCfgs,
}

/// Target cfgs per owner platform (`None` for the host), as `(name,
/// value)` pairs in rustc's order, without `debug_assertions`.
type ScriptCfgs = BTreeMap<Option<Handle<TargetDef>>, Vec<(String, Option<String>)>>;

/// Probe the cfgs of every platform a build script configures. Done
/// once per build, before dispatch, so parallel runs do not race on the
/// probe cache file. A build without runs probes nothing.
fn probe_script_cfgs(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    units: &UnitGraph,
) -> Result<ScriptCfgs> {
    use crate::manifest::platform::{ProbeTarget, probe};
    use cargo_platform::Cfg;
    let mut out = ScriptCfgs::new();
    for (_, run) in units.runs() {
        let key = match units.get(run.owner).platform {
            Platform::Host => None,
            Platform::Cross(t) => Some(t),
        };
        if out.contains_key(&key) {
            continue;
        }
        let target = match key {
            None => ProbeTarget::Host,
            Some(t) => {
                let def = model.targets.get(t).ok_or_else(|| {
                    Error::Compile(format!(
                        "build-script cfg probe: target handle {t:?} not in the model"
                    ))
                })?;
                ProbeTarget::for_target(def, &resolved.project_root)
            }
        };
        let platform = probe(
            &ctx.rustc_info,
            &target,
            Some(&ctx.layout.target_cfg_cache()),
        )?;
        let cfgs = platform
            .cfgs
            .into_iter()
            .map(|c| match c {
                Cfg::Name(n) => (n.name, None),
                Cfg::KeyPair(k, v) => (k.name, Some(v)),
            })
            .collect();
        out.insert(key, cfgs);
    }
    Ok(out)
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
    units: &UnitGraph,
    view: &ResultView<'_>,
    target: Handle<TargetDef>,
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
    // The DAG has a ConfigCrate node only for targets with a unit that
    // links it, and lowering hashes exactly those.
    let metadata = units.config_metadata(target).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: ConfigCrate({target:?}) has no config crate hash; \
             no unit for this target links it (scheduler bug)"
        ))
    })?;
    let (_name, rlib_path, was_cached) = crate::generated::config_crate::ensure_config_crate(
        ctx,
        model,
        resolved,
        target,
        metadata,
        sysroot_dir,
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
) -> Result<NodeResult> {
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
    // Computed from the inputs' slots, so a cache hit carries it too.
    let link_search = crate::compile::command_builder::closure_link_search(units, unit, view);
    Ok(NodeResult {
        primary: Some(out_path),
        was_cached,
        data: if link_search.is_empty() {
            NodeData::None
        } else {
            NodeData::LinkSearch(link_search.into())
        },
    })
}

/// Run one build script for its owner; see [`crate::build_script::run`].
///
/// The script's environment is assembled from the owner unit, the
/// probed cfgs of its platform (`CARGO_CFG_*`, plus `debug_assertions`
/// when the owner's opt-level is 0, as rustc then enables them), the
/// flags Quon passes to every crate of the platform (the panic
/// strategy), and the `links` metadata of the owner's direct deps
/// (`DEP_*`).
#[allow(clippy::too_many_arguments)]
fn dispatch_run_build_script(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    units: &UnitGraph,
    view: &ResultView<'_>,
    script_cfgs: &ScriptCfgs,
    r: RunId,
    stderr: &mut Vec<u8>,
) -> Result<NodeResult> {
    use crate::build_script::{ScriptEnvInput, ScriptPlatform, ScriptTarget, script_env};
    use crate::compile::extern_map::ArtifactLookup;
    use crate::unit::UnitProfile;

    let run = units.run(r);
    let owner = units.get(run.owner);
    let what = format!(
        "build script of {}",
        units.label(run.owner, model, resolved)
    );
    let binary = view.unit(run.script).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: {what} ran before its script compiled; DAG edge missing?"
        ))
    })?;
    let run_dir = units.run_dir(r, &ctx.layout, model, resolved)?;
    let out_dir = run_dir.join("out");
    let (opt_level, debug_info) = match owner.profile {
        UnitProfile::Host => (
            crate::compile::HOST_OPT_LEVEL,
            crate::compile::HOST_DEBUG_INFO,
        ),
        UnitProfile::Active => (resolved.profile.opt_level, resolved.profile.debug_info),
    };
    let spec_file;
    let quon_sysroot;
    let mut target_rustflags = Vec::new();
    let platform = match owner.platform {
        Platform::Host => ScriptPlatform::Host,
        Platform::Cross(t) => {
            let target = model.targets.get(t).ok_or_else(|| {
                Error::Compile(format!("{what}: target handle {t:?} not in the model"))
            })?;
            // Every crate of the target gets the sysroot's panic
            // strategy (see the command builder), so probes must too.
            if let Some(s) = target.panic_strategy {
                target_rustflags.push(format!("-Cpanic={}", s.as_str()));
            }
            spec_file = crate::compile::rustc::target_spec_file(&target.spec, target.builtin);
            quon_sysroot = view
                .sysroot(t)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| ctx.layout.sysroot_dir(target));
            ScriptPlatform::Cross {
                target: match &spec_file {
                    Some(path) => ScriptTarget::Custom(path),
                    None => ScriptTarget::Builtin(&target.spec),
                },
                quon_sysroot: &quon_sysroot,
                opt_level,
                debug_info,
            }
        }
    };
    let cfg_key = match owner.platform {
        Platform::Host => None,
        Platform::Cross(t) => Some(t),
    };
    let mut target_cfgs = script_cfgs.get(&cfg_key).cloned().ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: {what} has no probed target cfgs (scheduler bug)"
        ))
    })?;
    if opt_level == quon_model::OptLevel::Zero {
        target_cfgs.push(("debug_assertions".into(), None));
    }
    let mut dep_metadata = Vec::new();
    for dep_run in units.links_dep_runs(r) {
        let out = view.build_script(dep_run).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: {what} ran before the build script of {} finished; \
                 DAG edge missing?",
                units.label(units.run(dep_run).owner, model, resolved)
            ))
        })?;
        let links = units.run(dep_run).package.links.clone().unwrap_or_default();
        dep_metadata.push((links, out.metadata.clone()));
    }
    let cargo = crate::vendor::cargo_cmd::resolve_cargo();
    let mut env = script_env(&ScriptEnvInput {
        package: &run.package,
        features: &owner.features,
        platform,
        out_dir: &out_dir,
        host_triple: &ctx.rustc_info.host_triple,
        toolchain_sysroot: &ctx.rustc_info.sysroot,
        cargo: &cargo,
        num_jobs: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        target_cfgs: &target_cfgs,
        target_rustflags: &target_rustflags,
        dep_metadata: &dep_metadata,
    });
    // `q.env(..)`, as Cargo's `[env]` reaches build scripts. What Quon
    // sets for the script keeps Quon's value.
    for (k, v) in &model.env {
        env.entry(k.clone()).or_insert_with(|| v.into());
    }
    let (out, was_cached) = crate::build_script::run::run_build_script(
        ctx,
        crate::build_script::run::RunInput {
            what,
            key: units.run_key(r, &ctx.layout, model, resolved)?,
            binary,
            env,
            manifest_dir: &run.package.manifest_dir,
            run_dir: &run_dir,
            has_links: run.package.links.is_some(),
            // Warnings from vendored scripts are noise the user cannot
            // act on; `-v` shows them, as in Cargo.
            show_warnings: ctx.verbose || !owner.settings.cap_lints,
        },
        stderr,
    )?;
    Ok(NodeResult {
        primary: None,
        was_cached,
        data: NodeData::BuildScript(out),
    })
}

fn dispatch_esp(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    view: &ResultView<'_>,
    esp_handle: Handle<EspDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let (path, was_cached) =
        crate::generated::esp::ensure_esp(ctx, model, resolved, esp_handle, view, stdout, stderr)?;
    Ok((Some(path), was_cached))
}

fn dispatch_image(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    view: &ResultView<'_>,
    image_handle: Handle<ImageDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutcome> {
    let ictx = ImageCtx {
        ctx,
        model,
        resolved,
        artifacts: view,
    };
    let (path, was_cached) = build_image(image_handle, &ictx, stdout, stderr)?;
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
        DagNode::RunBuildScript(r) => format!(
            "build script of {}",
            units.label(units.run(r).owner, model, resolved)
        ),
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

/// What `node` does, for progress output; see [`JobInfo`].
fn node_info(node: DagNode, model: &BuildModel, units: &UnitGraph) -> JobInfo {
    let target_name = |h: Handle<TargetDef>| {
        model
            .targets
            .get(h)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| format!("{h:?}"))
    };
    let named = |kind: JobKind, name: Option<&str>, h: &dyn std::fmt::Debug| JobInfo {
        kind,
        name: name.map_or_else(|| format!("{h:?}"), str::to_string),
        detail: String::new(),
    };
    match node {
        DagNode::Sysroot(t) => named(JobKind::Sysroot, Some(&target_name(t)), &t),
        DagNode::ConfigCrate(t) => named(JobKind::ConfigCrate, Some(&target_name(t)), &t),
        DagNode::Compile(u) => {
            let unit = units.get(u);
            if unit.is_build_script {
                return named(JobKind::BuildScript, Some(&unit.package.name), &u);
            }
            let platform = match unit.platform {
                Platform::Host => "host".to_string(),
                Platform::Cross(t) => target_name(t),
            };
            JobInfo {
                kind: JobKind::Crate,
                name: unit.name.clone(),
                detail: format!("v{} ({platform})", unit.package.version),
            }
        }
        DagNode::RunBuildScript(r) => {
            let owner = units.get(units.run(r).owner);
            named(JobKind::BuildScript, Some(&owner.package.name), &r)
        }
        DagNode::Rule(h) => named(
            JobKind::Rule,
            model.rules.get(h).map(|r| r.name.as_str()),
            &h,
        ),
        DagNode::Esp(h) => named(JobKind::Esp, model.esps.get(h).map(|e| e.name.as_str()), &h),
        DagNode::Image(h) => named(
            JobKind::Image,
            model.images.get(h).map(|i| i.name.as_str()),
            &h,
        ),
        DagNode::BuildStep(h) => named(
            JobKind::BuildStep,
            model.build_steps.get(h).map(|s| s.name.as_str()),
            &h,
        ),
    }
}

impl JobDispatcher for PipelineDispatcher<'_> {
    fn dispatch(&self, node: NodeId, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> Result<()> {
        let view = self.view();
        let (ctx, model, resolved) = (self.ctx, self.model, self.resolved);
        let plain = |(primary, was_cached): NodeOutcome| NodeResult {
            primary,
            was_cached,
            data: NodeData::None,
        };
        let result = match self.dag.nodes[node as usize] {
            DagNode::Sysroot(target) => {
                plain(dispatch_sysroot(ctx, model, target, stdout, stderr)?)
            }
            DagNode::ConfigCrate(target) => plain(dispatch_config_crate(
                ctx, model, resolved, self.units, &view, target, stderr,
            )?),
            // rustc writes everything to stderr; the worker pool
            // flushes it per job.
            DagNode::Compile(unit) => {
                dispatch_compile(ctx, model, resolved, self.units, &view, unit, stderr)?
            }
            DagNode::RunBuildScript(r) => dispatch_run_build_script(
                ctx,
                model,
                resolved,
                self.units,
                &view,
                self.script_cfgs,
                r,
                stderr,
            )?,
            DagNode::Esp(handle) => plain(dispatch_esp(
                ctx, model, resolved, &view, handle, stdout, stderr,
            )?),
            DagNode::Image(handle) => plain(dispatch_image(
                ctx, model, resolved, &view, handle, stdout, stderr,
            )?),
            DagNode::Rule(handle) => plain(dispatch_rule(
                ctx, model, resolved, self.rules, &view, handle, stdout, stderr,
            )?),
            DagNode::BuildStep(handle) => plain(dispatch_build_step(
                ctx, model, resolved, &view, handle, stdout, stderr,
            )?),
        };
        self.results[node as usize].set(result).map_err(|_| {
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

    fn info(&self, node: NodeId) -> JobInfo {
        node_info(self.dag.nodes[node as usize], self.model, self.units)
    }

    fn was_cached(&self, node: NodeId) -> bool {
        self.results[node as usize]
            .get()
            .is_some_and(|r| r.was_cached)
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
/// handed to `reporter` as each job completes. This prevents
/// interleaved rustc output from parallel compilations.
pub fn execute_pipeline(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    vendor_ctx: &VendorBuildContext,
    rules: &RuleRegistry,
    workers: usize,
    reporter: &mut dyn Reporter,
) -> Result<BuildSummary> {
    let prepared = Prepared::new(ctx, model, resolved, vendor_ctx)?;
    let dispatcher = prepared.dispatcher(ctx, model, resolved, rules);
    let pool = WorkerPool::new(workers);
    pool.execute_with(&prepared.dag, &dispatcher, reporter)?;

    Ok(summarize(&prepared.dag, &prepared.results, model))
}

/// What one model's build needs before dispatch: its units, DAG,
/// probed cfgs and an empty result slot per node.
struct Prepared {
    units: UnitGraph,
    dag: DagType,
    node_of_unit: Vec<NodeId>,
    script_cfgs: ScriptCfgs,
    results: Vec<OnceLock<NodeResult>>,
}

impl Prepared {
    fn new(
        ctx: &CompileCtx,
        model: &BuildModel,
        resolved: &ResolvedConfig,
        vendor_ctx: &VendorBuildContext,
    ) -> Result<Self> {
        let units = crate::unit::lower(model, resolved, vendor_ctx)?;
        let dag = build_dag(resolved, model, &units)?;
        let node_of_unit = results::node_of_unit(&dag, &units);
        let script_cfgs = probe_script_cfgs(ctx, model, resolved, &units)?;
        let results = (0..dag.len()).map(|_| OnceLock::new()).collect();
        Ok(Self {
            units,
            dag,
            node_of_unit,
            script_cfgs,
            results,
        })
    }

    fn dispatcher<'a>(
        &'a self,
        ctx: &'a CompileCtx,
        model: &'a BuildModel,
        resolved: &'a ResolvedConfig,
        rules: &'a RuleRegistry,
    ) -> PipelineDispatcher<'a> {
        PipelineDispatcher {
            ctx,
            model,
            resolved,
            rules,
            units: &self.units,
            dag: &self.dag,
            node_of_unit: &self.node_of_unit,
            results: &self.results,
            script_cfgs: &self.script_cfgs,
        }
    }
}

/// Whether the summary counts `node`: rule nodes are not cacheable, and
/// Quon neither builds nor caches a prebuilt sysroot.
fn counted(node: DagNode, model: &BuildModel) -> bool {
    match node {
        DagNode::Rule(_) => false,
        DagNode::Sysroot(t) => model.targets.get(t).is_some_and(TargetDef::builds_sysroot),
        _ => true,
    }
}

/// Assemble the build summary from the result slots after the pool
/// has joined. Rule nodes are not cacheable and are not counted, and
/// neither is a prebuilt sysroot, which Quon neither builds nor caches.
fn summarize(dag: &DagType, results: &[OnceLock<NodeResult>], model: &BuildModel) -> BuildSummary {
    let mut summary = BuildSummary::default();
    for (node, slot) in dag.nodes.iter().zip(results) {
        let Some(result) = slot.get() else { continue };
        if counted(*node, model) {
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
    use crate::compile::{BuildLayout, RustcInfo};
    use quon_model::{
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
        CompileCtx::new(layout, Arc::new(info))
    }

    fn make_minimal_model_and_resolved() -> (BuildModel, ResolvedConfig) {
        let mut model = BuildModel::default();
        let target = TargetDef {
            name: "x86_64-unknown-none".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            build_std: false,
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
                target: TargetSpec::Cross(target_handle),
                opt_level: quon_model::OptLevel::Zero,
                debug_info: false,
                lto: None,
                run_binary: None,
                runner: None,
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
            build_script: None,
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
                TargetSpec::Cross(resolved.profile.cross_target().unwrap()),
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
        use quon_model::{CrateDef, CrateType, GroupDef};
        let (mut model, mut resolved) = make_minimal_model_and_resolved();
        let target = resolved.profile.cross_target().unwrap();
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
                cargo: None,
            },
        );
        resolved.crates.push(quon_model::ResolvedCrateRef {
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
            &mut PlainReporter {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
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
    // Run with: cargo test -p quon-core -- --ignored execute_pipeline
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
                &mut PlainReporter {
                    stdout: &mut out,
                    stderr: &mut err,
                },
            )
            .expect("first run should succeed");
        }
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
                &mut PlainReporter {
                    stdout: &mut out,
                    stderr: &mut err,
                },
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
            &mut PlainReporter {
                stdout: &mut out1,
                stderr: &mut err1,
            },
        )
        .expect("first run");

        let target = model
            .targets
            .get(resolved.profile.cross_target().unwrap())
            .expect("target");
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
            &mut PlainReporter {
                stdout: &mut out2,
                stderr: &mut err2,
            },
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
            &mut PlainReporter {
                stdout: &mut stdout,
                stderr: &mut stderr,
            },
        )
        .expect("pipeline should succeed");

        // Verify the binary was produced.
        let target = model
            .targets
            .get(resolved.profile.cross_target().unwrap())
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
        use quon_model::{CrateDef, CrateType, GroupDef, TargetSpec};
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
            hello_dir.join("quon.ld"),
            b"ENTRY(_start)\n\
              SECTIONS {\n\
              \t.text : { *(.text .text*) }\n\
              \t.rodata : { *(.rodata .rodata*) }\n\
              \t.data : { *(.data .data*) }\n\
              \t.bss : { *(.bss .bss*) }\n\
              }\n",
        )
        .expect("write quon.ld");

        // --- Build model ---
        let mut model = BuildModel::default();

        let target_def = TargetDef {
            name: "x86_64-unknown-none".into(),
            spec: "x86_64-unknown-none".into(),
            builtin: true,
            build_std: false,
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
            linker_script: Some("crates/hello/quon.ld".into()),
            group: group_handle,
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
            cargo: None,
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
                target: TargetSpec::Cross(target_handle),
                opt_level: quon_model::OptLevel::Zero,
                debug_info: false,
                lto: None,
                run_binary: None,
                runner: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            },
            options: BTreeMap::new(),
            crates: vec![quon_model::ResolvedCrateRef {
                handle: crate_handle,
                target: target_handle,
                host: false,
            }],
            build_dir: build_dir.clone(),
            project_root: tmp.to_path_buf(),
        };

        // --- CompileCtx ---
        let layout = BuildLayout::new(build_dir.clone(), "testproj");
        let ctx = CompileCtx::new(layout, Arc::new(info.clone()));
        // These tests are about scheduling, not the sysroot, which
        // `sysroot::tests` builds from scratch.
        crate::testing::seed_sysroots(&build_dir, &model, info.rustc_arg.as_ref())
            .expect("seed sysroot");

        (ctx, model, resolved)
    }
}
