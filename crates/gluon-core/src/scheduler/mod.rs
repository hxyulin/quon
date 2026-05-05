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
//! helper. The dispatch protocol — clone the snapshot, run the helper
//! lock-free, merge the result — is documented in
//! `docs/src/architecture/concurrency.md`.

pub mod dag;
mod snapshot;
pub mod worker;

pub use dag::{Dag, DagNode, NodeId, build_dag};
pub use worker::{BuildSummary, JobDispatcher, WorkerPool};

use crate::compile::vendor_command::{VendorCompileInput, build_vendor_rustc_command};
use crate::compile::{CompileCrateInput, CompileCtx, compile_crate};
use crate::error::{Diagnostic, Error, Result};
use crate::image::{ImageCtx, build_image};
use crate::rule::{RuleCtx, RuleRegistry};
use crate::vendor::cross::context::{VendorBuildContext, VendorBuildId};
use dag::Dag as DagType;
use gluon_model::{
    BuildModel, BuildStepDef, CrateDef, EspDef, Handle, ImageDef, ResolvedConfig, RuleDef,
    TargetDef, TargetSpec,
};
use snapshot::{ArtifactsSnapshot, NodeOutput};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// PipelineDispatcher
// ---------------------------------------------------------------------------

/// Maps each `DagNode` variant to the appropriate helper or compile call.
///
/// Implements `JobDispatcher` so it can be driven by `WorkerPool`. All
/// shared state is behind `Mutex` so workers can synchronise without
/// holding broad locks across compilation.
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
    /// Single canonical-state mutex. Stage 8 collapses the four per-map
    /// mutexes (sysroots, artifacts, esps, images) into this bundle so
    /// per-node helpers see only `&ArtifactsSnapshot` clones — never
    /// shared mutable state. The dispatch arms are the sole mutation
    /// point: lock once to read-clone before the helper, lock once to
    /// merge the helper's `NodeOutput` afterwards.
    state: &'a Mutex<ArtifactsSnapshot>,
    /// Lock-free counter for nodes that ran rustc to completion.
    /// `Rule` nodes are not counted (they are not cacheable in MVP-M).
    built: &'a AtomicUsize,
    /// Lock-free counter for nodes that returned via the cache fast-path.
    cached: &'a AtomicUsize,
}

impl<'a> PipelineDispatcher<'a> {
    /// Increment the appropriate counter for a cacheable node.
    fn record(&self, was_cached: bool) {
        if was_cached {
            self.cached.fetch_add(1, Ordering::Relaxed);
        } else {
            self.built.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Lock a mutex, converting a poisoned-lock panic into a gluon [`Error`].
///
/// The scheduler shares several `Mutex`-guarded maps across worker
/// threads. If any thread panics while holding a lock the mutex is
/// poisoned and all subsequent `lock()` calls would panic. This helper
/// converts that into a recoverable `Error::Config` so the build fails
/// with a diagnostic instead of an abort.
fn lock_or_poison<'a, T>(
    mutex: &'a std::sync::Mutex<T>,
    name: &str,
) -> Result<std::sync::MutexGuard<'a, T>> {
    mutex
        .lock()
        .map_err(|_| Error::Config(format!("scheduler: {name} mutex poisoned")))
}

// ---------------------------------------------------------------------------
// Per-node dispatch helpers
//
// Each `dispatch_*` is a pure function over an immutable snapshot — no
// mutex, no shared state. The dispatch arm acquires the state lock
// twice: once before the helper to clone the snapshot, once after to
// merge the helper's `NodeOutput`. Helpers must never see a `Mutex`.
// ---------------------------------------------------------------------------

/// `_snapshot` is unused today (Sysroot has no inputs from prior
/// nodes) but is part of the dispatch-helper signature shape so all
/// per-variant fns are uniform.
fn dispatch_sysroot(
    ctx: &CompileCtx,
    model: &BuildModel,
    _snapshot: &ArtifactsSnapshot,
    target: Handle<TargetDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutput> {
    let (path, was_cached) =
        crate::sysroot::ensure_sysroot_for_node(ctx, model, target, stdout, stderr)?;
    Ok(NodeOutput::Sysroot {
        target,
        path,
        was_cached,
    })
}

fn dispatch_config_crate(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    snapshot: &ArtifactsSnapshot,
    target: Handle<TargetDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutput> {
    // The DAG wires ConfigCrate(target) → Sysroot(target) so this
    // lookup must always succeed.
    let sysroot_dir = snapshot.sysroots.get(&target).cloned().ok_or_else(|| {
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
        &sysroot_dir,
        stdout,
        stderr,
    )?;
    Ok(NodeOutput::ConfigCrate {
        target,
        rlib_path,
        was_cached,
    })
}

fn dispatch_crate(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    snapshot: &ArtifactsSnapshot,
    crate_handle: Handle<CrateDef>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutput> {
    let crate_ref = resolved
        .crates
        .iter()
        .find(|r| r.handle == crate_handle)
        .ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: Crate node references handle {:?} not in resolved.crates",
                crate_handle
            ))
        })?;

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
    let sysroot_dir: Option<PathBuf> = if crate_ref.host || group_no_sysroot {
        None
    } else {
        Some(
            snapshot
                .sysroots
                .get(&crate_ref.target)
                .cloned()
                .ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: Crate({:?}) ran before its Sysroot({:?}) dependency \
                         completed; DAG edge missing?",
                        crate_handle, crate_ref.target,
                    ))
                })?,
        )
    };

    let (out_path, was_cached) = compile_crate(
        ctx,
        CompileCrateInput {
            model,
            resolved,
            crate_ref,
            artifacts: &snapshot.artifacts,
            sysroot_dir: sysroot_dir.as_deref(),
        },
        stderr,
    )?;

    Ok(NodeOutput::Crate {
        handle: crate_handle,
        out_path,
        was_cached,
    })
}

fn dispatch_esp(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    snapshot: &ArtifactsSnapshot,
    esp_handle: Handle<EspDef>,
    stdout: &mut Vec<u8>,
) -> Result<NodeOutput> {
    let (path, was_cached) = crate::generated::esp::ensure_esp(
        ctx,
        model,
        resolved,
        esp_handle,
        &snapshot.artifacts,
        stdout,
    )?;
    Ok(NodeOutput::Esp {
        handle: esp_handle,
        path,
        was_cached,
    })
}

fn dispatch_image(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    snapshot: &ArtifactsSnapshot,
    image_handle: Handle<ImageDef>,
    stdout: &mut Vec<u8>,
) -> Result<NodeOutput> {
    let ictx = ImageCtx {
        ctx,
        model,
        resolved,
        artifacts: &snapshot.artifacts,
        esp_dirs: &snapshot.esps,
    };
    let (path, was_cached) = build_image(image_handle, &ictx, stdout)?;
    Ok(NodeOutput::Image {
        handle: image_handle,
        path,
        was_cached,
    })
}

// 8 args is one above clippy's default limit; bundling would obscure
// the uniform `(ctx, model, resolved, snapshot, handle, stdout, stderr)`
// shape shared with the other dispatch_* helpers. The extra arg here
// is the `rules: &RuleRegistry` only Rule needs.
#[allow(clippy::too_many_arguments)]
fn dispatch_rule(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    rules: &RuleRegistry,
    snapshot: &ArtifactsSnapshot,
    rule_handle: Handle<RuleDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutput> {
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
        artifacts: &snapshot.artifacts,
        image_paths: &snapshot.images,
    };
    rules.dispatch(&rule_ctx, rule, stdout, stderr)?;
    Ok(NodeOutput::Rule)
}

fn dispatch_build_step(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    snapshot: &ArtifactsSnapshot,
    handle: Handle<BuildStepDef>,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutput> {
    let (out_path, was_cached) = crate::compile::build_step::execute_build_step(
        ctx,
        model,
        resolved,
        handle,
        &snapshot.artifacts,
        stdout,
        stderr,
    )?;
    Ok(NodeOutput::BuildStep {
        handle,
        out_path,
        was_cached,
    })
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

/// Build the `extra_sources` list for a vendor crate's freshness query.
///
/// Vendor crates use *stable* on-disk paths for both their `--sysroot`
/// and their `--extern` dep rlibs — there's no extra-filename hash in
/// the way cargo would normally salt these. That means a rebuild of
/// the sysroot or of a dep produces a new file at the same path; the
/// compile-time argv (and therefore [`crate::cache::Cache::is_fresh`]'s
/// `argv_hash`) is unchanged. Rustc's depfile-driven source list
/// won't surface the change either: vendor builds emit a depfile but
/// don't pass `-Zbinary-dep-depinfo`, so the depfile only lists `.rs`
/// files — no rlibs.
///
/// Threading the dep rlibs and sysroot rlibs through `extra_sources`
/// makes the cache layer's per-input two-tier (mtime + sha256) check
/// participate in the freshness decision. A content change in any
/// dep rlib or sysroot rlib then invalidates the consumer's entry on
/// the next build, just like a `.rs` source edit would.
///
/// Output is sorted by path so the cache record is deterministic
/// across invocations — see the project-wide ordering invariant in
/// `CLAUDE.md`.
fn collect_vendor_extra_sources(
    extern_artifacts: &BTreeMap<String, PathBuf>,
    sysroot_lib_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = extern_artifacts.values().cloned().collect();

    // Walk just the immediate children of `<sysroot>/lib/rustlib/<target>/lib`
    // and pick up the rlibs (typically core, alloc, compiler_builtins).
    // Missing dir is silently tolerated: on a cold build the dir may
    // not exist yet, but the DAG guarantees the Sysroot node ran first
    // for cross builds, so in practice this is just defensive.
    if let Some(lib_dir) = sysroot_lib_dir
        && let Ok(entries) = std::fs::read_dir(lib_dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("rlib") {
                sources.push(path);
            }
        }
    }

    sources.sort();
    sources
}

/// Compile one vendored crate via gluon-driven rustc.
///
/// Mirrors [`dispatch_crate`]'s shape: looks up the build descriptor,
/// resolves dep `--extern` paths from the snapshot, derives target/sysroot
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
    snapshot: &ArtifactsSnapshot,
    id: VendorBuildId,
    stderr: &mut Vec<u8>,
) -> Result<NodeOutput> {
    let build = vendor_ctx.get(id).ok_or_else(|| {
        Error::Compile(format!(
            "scheduler: VendorCrate({id}) not found in vendor context (scheduler bug)",
        ))
    })?;

    // --- Resolve dep --extern paths from snapshot.vendor_artifacts ---
    // The DAG wires every dep edge into a vendor → vendor scheduler edge,
    // so each dep must already be in vendor_artifacts by the time this
    // arm runs. A miss is a scheduler bug, not a user mistake.
    let mut extern_artifacts: BTreeMap<String, PathBuf> = BTreeMap::new();
    for edge in &build.deps {
        let dep_id = vendor_ctx.lookup(edge.target, &edge.extern_name).ok_or_else(|| {
            Error::Compile(format!(
                "scheduler: vendor build {id} ({}) dep edge `{}` did not resolve in \
                 vendor_ctx (lowering bug — DAG was built but lookup now fails)",
                build.crate_id.name, edge.extern_name,
            ))
        })?;
        let dep_path = snapshot.vendor_artifacts.get(&dep_id).cloned().ok_or_else(|| {
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
    // Cross: triple from TargetDef::spec, sysroot from snapshot, out_dir
    // under cross/<target>/<profile>/vendor-deps/.
    //
    // `sysroot_lib_dir` is captured separately from `sysroot_owned`
    // because the freshness layer needs to enumerate the sysroot
    // *libdir* (`<sysroot>/lib/rustlib/<target>/lib`) — see
    // [`collect_vendor_extra_sources`] for why.
    let (target_triple, target_triple_builtin, sysroot_owned, sysroot_lib_dir, out_dir) =
        match build.target {
            TargetSpec::Host => (
                None,
                false,
                None::<PathBuf>,
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
                let sysroot = snapshot.sysroots.get(&target_handle).cloned().ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: vendor build {id} ({}) ran before its Sysroot({:?}) \
                         dependency completed; DAG edge missing?",
                        build.crate_id.name, target_handle,
                    ))
                })?;
                let sysroot_lib = ctx.layout.sysroot_lib_dir(target);
                let out_dir = ctx.layout.cross_vendor_dep_dir(
                    target,
                    &resolved.profile,
                    &build.cargo_target_name,
                );
                (
                    Some(target.spec.clone()),
                    target.builtin,
                    Some(sysroot),
                    Some(sysroot_lib),
                    out_dir,
                )
            }
        };

    // --- Assemble rustc command ---
    let input = VendorCompileInput {
        build,
        extern_artifacts: &extern_artifacts,
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
    let argv_hash = builder.hash();
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

    // Fold dep rlibs and sysroot rlibs into the freshness query so a
    // sysroot or dep rebuild propagates to this vendor crate's cache
    // entry. See [`collect_vendor_extra_sources`] for the rationale.
    let extra_sources = collect_vendor_extra_sources(&extern_artifacts, sysroot_lib_dir.as_deref());

    let (out_path, was_cached) = crate::compile::run_compile_and_cache(
        ctx,
        builder,
        argv_hash,
        cache_key,
        output_path,
        depfile_path,
        Some(&out_dir),
        extra_sources,
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

    Ok(NodeOutput::VendorCrate {
        id,
        out_path,
        was_cached,
    })
}

impl<'a> JobDispatcher for PipelineDispatcher<'a> {
    fn dispatch(&self, node: NodeId, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> Result<()> {
        let node_kind = self.dag.nodes[node as usize];
        match node_kind {
            // ------------------------------------------------------------------
            // Sysroot — build the custom sysroot for the target triple.
            // ------------------------------------------------------------------
            DagNode::Sysroot(target) => {
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_sysroot(
                    self.ctx, self.model, &snapshot, target, stdout, stderr,
                )?;
                let was_cached = lock_or_poison(self.state, "state")?
                    .merge(output)
                    .expect("Sysroot output is cacheable");
                self.record(was_cached);
                Ok(())
            }

            // ------------------------------------------------------------------
            // ConfigCrate — generate + compile `<project>_config` for a target.
            // ------------------------------------------------------------------
            DagNode::ConfigCrate(target) => {
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_config_crate(
                    self.ctx,
                    self.model,
                    self.resolved,
                    &snapshot,
                    target,
                    stdout,
                    stderr,
                )?;
                let was_cached = lock_or_poison(self.state, "state")?
                    .merge(output)
                    .expect("ConfigCrate output is cacheable");
                self.record(was_cached);
                Ok(())
            }

            // ------------------------------------------------------------------
            // Crate — compile a user-declared crate (host or cross).
            // ------------------------------------------------------------------
            DagNode::Crate(crate_handle) => {
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_crate(
                    self.ctx, self.model, self.resolved, &snapshot, crate_handle, stderr,
                )?;
                let was_cached = lock_or_poison(self.state, "state")?
                    .merge(output)
                    .expect("Crate output is cacheable");
                self.record(was_cached);
                // Crate compiles never write to stdout — rustc emits everything
                // to stderr, which the worker pool flushes per-job.
                let _ = stdout;
                Ok(())
            }

            // ------------------------------------------------------------------
            // Esp — assemble an EFI System Partition directory from
            // built crate artifacts. See generated::esp::ensure_esp.
            // ------------------------------------------------------------------
            DagNode::Esp(esp_handle) => {
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_esp(
                    self.ctx, self.model, self.resolved, &snapshot, esp_handle, stdout,
                )?;
                let was_cached = lock_or_poison(self.state, "state")?
                    .merge(output)
                    .expect("Esp output is cacheable");
                self.record(was_cached);
                let _ = stderr;
                Ok(())
            }

            // ------------------------------------------------------------------
            // Image — assemble a bootable disk image. See image::build_image.
            // ------------------------------------------------------------------
            DagNode::Image(image_handle) => {
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_image(
                    self.ctx, self.model, self.resolved, &snapshot, image_handle, stdout,
                )?;
                let was_cached = lock_or_poison(self.state, "state")?
                    .merge(output)
                    .expect("Image output is cacheable");
                self.record(was_cached);
                let _ = stderr;
                Ok(())
            }

            // ------------------------------------------------------------------
            // Rule — run a user-declared rule through the registry.
            // ------------------------------------------------------------------
            DagNode::Rule(rule_handle) => {
                // Rule nodes are not cacheable in MVP-M — no counter update.
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_rule(
                    self.ctx,
                    self.model,
                    self.resolved,
                    self.rules,
                    &snapshot,
                    rule_handle,
                    stdout,
                    stderr,
                )?;
                // merge() returns None for Rule — no counter to update.
                lock_or_poison(self.state, "state")?.merge(output);
                Ok(())
            }

            // ------------------------------------------------------------------
            // BuildStep — invoke a host-bin runner with curated env vars.
            // ------------------------------------------------------------------
            DagNode::BuildStep(handle) => {
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_build_step(
                    self.ctx, self.model, self.resolved, &snapshot, handle, stdout, stderr,
                )?;
                let was_cached = lock_or_poison(self.state, "state")?
                    .merge(output)
                    .expect("BuildStep output is cacheable");
                self.record(was_cached);
                Ok(())
            }

            // ------------------------------------------------------------------
            // VendorCrate — compile a vendored external crate via gluon-driven
            // rustc against the per-target sysroot. Dep `--extern` paths
            // come from `snapshot.vendor_artifacts` (populated by prior
            // VendorCrate nodes in topological order); cross builds also
            // pull their sysroot from `snapshot.sysroots`.
            // ------------------------------------------------------------------
            DagNode::VendorCrate(id) => {
                let snapshot = lock_or_poison(self.state, "state")?.clone();
                let output = dispatch_vendor_crate(
                    self.ctx,
                    self.model,
                    self.resolved,
                    self.vendor_ctx,
                    &snapshot,
                    id,
                    stderr,
                )?;
                let was_cached = lock_or_poison(self.state, "state")?
                    .merge(output)
                    .expect("VendorCrate output is cacheable");
                self.record(was_cached);
                let _ = stdout;
                Ok(())
            }
        }
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
/// `sysroots` and `artifacts` are `Mutex`-wrapped maps updated by the
/// dispatcher as each node completes. The DAG guarantees topological order:
/// a node's dependencies are always complete before the node itself runs, so
/// lookups into these maps are always safe (no missing-key panics in correct
/// usage).
///
/// ### stdout / stderr
///
/// Per-job output is buffered in `Vec<u8>` inside the worker pool and
/// flushed to `stdout`/`stderr` as each job completes. This prevents
/// interleaved rustc output from parallel compilations.
// Eight inputs is one above clippy's default threshold. Folding them
// into a `PipelineInputs` bundle would be the right long-term shape
// (and is queued for a follow-up refactor) but would touch every
// call site for an orthogonal concern; doing it here would muddy the
// vendor pipeline diff.
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

    let state: Mutex<ArtifactsSnapshot> = Mutex::new(ArtifactsSnapshot::default());
    // `Relaxed` is sufficient: we only need atomicity, not ordering
    // relative to other memory operations. The counters are read once
    // after `pool.execute` returns (a happens-before point already
    // established by the worker-pool join).
    let built = AtomicUsize::new(0);
    let cached = AtomicUsize::new(0);

    let dispatcher = PipelineDispatcher {
        ctx,
        model,
        resolved,
        rules,
        vendor_ctx,
        dag: &dag,
        state: &state,
        built: &built,
        cached: &cached,
    };

    let pool = WorkerPool::new(workers);
    pool.execute(&dag, &dispatcher, stdout, stderr)?;

    // Drain the snapshot into the summary. The mutex is uncontested
    // at this point (worker pool has joined), so `into_inner` is safe.
    let final_state = state
        .into_inner()
        .map_err(|_| Error::Config("scheduler: state mutex poisoned".into()))?;
    let build_step_paths: BTreeMap<_, _> = final_state
        .artifacts
        .build_steps_iter()
        .map(|(h, p)| (h, p.to_path_buf()))
        .collect();

    Ok(BuildSummary {
        built: built.load(Ordering::Relaxed),
        cached: cached.load(Ordering::Relaxed),
        esp_dirs: final_state.esps,
        image_paths: final_state.images,
        build_step_paths,
    })
}

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
            crate_id: CrateId { name: name.into(), version: ver.into() },
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
    fn dispatch_vendor_crate_missing_sysroot_for_cross_errors() {
        // Cross vendor build whose Sysroot isn't in the snapshot — this
        // is the "Sysroot ran after VendorCrate" failure mode.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ctx = make_ctx(tmp.path());
        let (model, resolved) = make_minimal_model_and_resolved();
        let target = resolved.profile.target;
        let mut vendor_ctx = VendorBuildContext::new();
        let id = vendor_ctx
            .add(vendor_lib("bitflags", "2.11.0", TargetSpec::Cross(target), vec![]))
            .expect("add ok");
        let snapshot = ArtifactsSnapshot::default(); // empty — no sysroot
        let mut stderr = Vec::<u8>::new();
        let err = dispatch_vendor_crate(
            &ctx, &model, &resolved, &vendor_ctx, &snapshot, id, &mut stderr,
        )
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
        // snapshot.vendor_artifacts — the "dep ran after parent" mode.
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
                    crate_id: CrateId { name: "dep".into(), version: "1.0.0".into() },
                    target: TargetSpec::Host,
                }],
            ))
            .expect("parent add ok");
        let snapshot = ArtifactsSnapshot::default(); // empty — dep not built
        let mut stderr = Vec::<u8>::new();
        let err = dispatch_vendor_crate(
            &ctx, &model, &resolved, &vendor_ctx, &snapshot, parent_id, &mut stderr,
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

    // -------------------------------------------------------------------
    // collect_vendor_extra_sources — pure helper, no rustc spawn
    // -------------------------------------------------------------------

    #[test]
    fn collect_vendor_extra_sources_empty_inputs_returns_empty() {
        // Host build with no deps and no sysroot — nothing to fold in.
        let extern_artifacts: BTreeMap<String, PathBuf> = BTreeMap::new();
        let result = collect_vendor_extra_sources(&extern_artifacts, None);
        assert!(result.is_empty(), "expected empty, got: {result:?}");
    }

    #[test]
    fn collect_vendor_extra_sources_sorts_dep_paths() {
        // Determinism: the BTreeMap iterates in key order ("alpha", "zeta"),
        // but the *paths* are sorted independently — pin the sort so a
        // future map switch can't silently reorder cache stamps.
        let mut extern_artifacts: BTreeMap<String, PathBuf> = BTreeMap::new();
        extern_artifacts.insert("zeta".into(), PathBuf::from("/build/aaa.rlib"));
        extern_artifacts.insert("alpha".into(), PathBuf::from("/build/zzz.rlib"));
        let result = collect_vendor_extra_sources(&extern_artifacts, None);
        assert_eq!(
            result,
            vec![
                PathBuf::from("/build/aaa.rlib"),
                PathBuf::from("/build/zzz.rlib"),
            ]
        );
    }

    #[test]
    fn collect_vendor_extra_sources_picks_up_sysroot_rlibs() {
        // Cross build: deps + sysroot dir with mixed file types. Only
        // top-level *.rlib entries should be folded in. The result must
        // be a sorted, deduplicated path list.
        let tmp = tempfile::tempdir().expect("tempdir");
        let lib_dir = tmp.path().join("lib");
        std::fs::create_dir_all(&lib_dir).expect("mkdir lib");

        // Top-level: two rlibs (must be picked up), one .so (skipped),
        // one .d depfile (skipped).
        let core_rlib = lib_dir.join("libcore-deadbeef.rlib");
        let alloc_rlib = lib_dir.join("liballoc-cafef00d.rlib");
        let dylib = lib_dir.join("libsomething.so");
        let depfile = lib_dir.join("libcore.d");
        for p in [&core_rlib, &alloc_rlib, &dylib, &depfile] {
            std::fs::write(p, b"x").expect("seed");
        }

        // Nested rlib (must NOT be picked up — read_dir is non-recursive).
        let nested = lib_dir.join("nested");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        std::fs::write(nested.join("libignored.rlib"), b"x").expect("seed nested");

        let mut extern_artifacts: BTreeMap<String, PathBuf> = BTreeMap::new();
        extern_artifacts.insert("bitflags".into(), PathBuf::from("/build/libbitflags.rlib"));

        let result = collect_vendor_extra_sources(&extern_artifacts, Some(&lib_dir));

        // Expected: dep + 2 sysroot rlibs, sorted. The .so and .d are
        // filtered; the nested rlib is invisible to read_dir's flat walk.
        let mut expected = vec![
            PathBuf::from("/build/libbitflags.rlib"),
            alloc_rlib.clone(),
            core_rlib.clone(),
        ];
        expected.sort();
        assert_eq!(result, expected);
    }

    #[test]
    fn collect_vendor_extra_sources_tolerates_missing_sysroot_dir() {
        // The DAG guarantees the Sysroot node ran before any cross
        // VendorCrate, so this branch is defensive — but a missing dir
        // must not panic or error: it just means no sysroot rlibs to
        // fold in (the cache will catch the mismatch on the next call
        // when the dir does exist).
        let mut extern_artifacts: BTreeMap<String, PathBuf> = BTreeMap::new();
        extern_artifacts.insert("dep".into(), PathBuf::from("/build/libdep.rlib"));
        let nope = PathBuf::from("/this/path/does/not/exist/lib");
        let result = collect_vendor_extra_sources(&extern_artifacts, Some(&nope));
        assert_eq!(result, vec![PathBuf::from("/build/libdep.rlib")]);
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
        let result = execute_pipeline(&ctx, &model, &resolved, &VendorBuildContext::new(), &rules, 1, &mut stdout, &mut stderr);

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
            execute_pipeline(&ctx, &model, &resolved, &VendorBuildContext::new(), &rules, 1, &mut out, &mut err)
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
            execute_pipeline(&ctx, &model, &resolved, &VendorBuildContext::new(), &rules, 1, &mut out, &mut err)
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
    /// the snapshot-based dispatch must be deterministic.
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
        let s1 = execute_pipeline(&ctx, &model, &resolved, &VendorBuildContext::new(), &rules, 1, &mut out1, &mut err1)
            .expect("first run");

        let target = model
            .targets
            .get(resolved.profile.target)
            .expect("target");
        let bin = ctx
            .layout
            .cross_final_dir(target, &resolved.profile)
            .join("hello");
        let bin1 = std::fs::read(&bin).expect("read first-run binary");

        // Second run — same fixture, same scheduler; cache should hit.
        let mut out2 = Vec::<u8>::new();
        let mut err2 = Vec::<u8>::new();
        let s2 = execute_pipeline(&ctx, &model, &resolved, &VendorBuildContext::new(), &rules, 1, &mut out2, &mut err2)
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
