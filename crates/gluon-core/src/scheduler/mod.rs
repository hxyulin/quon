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
//! [`helpers`] contains per-node work functions called by [`execute_pipeline`].
//!
//! [`execute_pipeline`] wires the above together: builds the DAG, runs the
//! worker pool, and dispatches each node to the appropriate helper.

pub mod dag;
mod snapshot;
pub mod worker;

pub use dag::{Dag, DagNode, NodeId, build_dag};
pub use worker::{BuildSummary, JobDispatcher, WorkerPool};

use crate::compile::{CompileCrateInput, CompileCtx, compile_crate};
use crate::error::{Error, Result};
use crate::image::{ImageCtx, build_image};
use crate::rule::{RuleCtx, RuleRegistry};
use dag::Dag as DagType;
use gluon_model::{BuildModel, Handle, ResolvedConfig, TargetDef};
use snapshot::{ArtifactsSnapshot, NodeOutput};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
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
                // Look up the sysroot for this config crate's target.
                // The DAG wires ConfigCrate(target) → Sysroot(target) so
                // this lookup must always succeed.
                let sysroot_dir = {
                    let guard = lock_or_poison(self.state, "state")?;
                    guard.sysroots.get(&target).cloned().ok_or_else(|| {
                        Error::Compile(
                            "scheduler: ConfigCrate ran before its Sysroot dependency; \
                                 DAG edge missing?"
                                .into(),
                        )
                    })?
                };
                let (_name, rlib_path, was_cached) = crate::generated::config_crate::ensure_config_crate(
                    self.ctx,
                    self.model,
                    self.resolved,
                    target,
                    &sysroot_dir,
                    stdout,
                    stderr,
                )?;
                lock_or_poison(self.state, "state")?
                    .artifacts
                    .set_config_crate(target, rlib_path);
                self.record(was_cached);
                Ok(())
            }

            // ------------------------------------------------------------------
            // Crate — compile a user-declared crate (host or cross).
            // ------------------------------------------------------------------
            DagNode::Crate(crate_handle) => {
                // Resolve the ResolvedCrateRef for this handle.
                let crate_ref = self
                    .resolved
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
                // ensures the Sysroot node for this target completed before
                // this Crate node was dispatched.
                //
                // Crates whose group sets `no_sysroot=true` skip this:
                // they have no Sysroot DAG edge, no sysroot was built for
                // their target, and `compile_crate` is configured to omit
                // `--sysroot` and the implicit core/alloc/compiler_builtins
                // externs.
                let group_no_sysroot = self
                    .model
                    .crates
                    .get(crate_handle)
                    .and_then(|c| self.model.groups.get(c.group))
                    .map(|g| g.no_sysroot)
                    .unwrap_or(false);
                let sysroot_dir: Option<PathBuf> = if crate_ref.host || group_no_sysroot {
                    None
                } else {
                    let guard = lock_or_poison(self.state, "state")?;
                    Some(
                        guard
                            .sysroots
                            .get(&crate_ref.target)
                            .cloned()
                            .ok_or_else(|| {
                                Error::Compile(format!(
                                    "scheduler: Crate({:?}) ran before its Sysroot({:?}) \
                                     dependency completed; DAG edge missing?",
                                    crate_handle, crate_ref.target,
                                ))
                            })?,
                    )
                };

                // Snapshot the ArtifactMap before releasing the lock.
                //
                // Rationale: we need a point-in-time view of all built
                // artifacts so compile_crate can wire --extern flags for the
                // crate's dependencies. Holding the lock across compile_crate
                // would serialise all parallel compilations on this single
                // mutex. Cloning a BTreeMap of a few dozen entries is cheap
                // compared to a rustc invocation.
                let artifacts_snapshot =
                    lock_or_poison(self.state, "state")?.artifacts.clone();

                let (out_path, was_cached) = compile_crate(
                    self.ctx,
                    CompileCrateInput {
                        model: self.model,
                        resolved: self.resolved,
                        crate_ref,
                        artifacts: &artifacts_snapshot,
                        sysroot_dir: sysroot_dir.as_deref(),
                    },
                    stderr,
                )?;

                lock_or_poison(self.state, "state")?
                    .artifacts
                    .insert(crate_handle, out_path);
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
                // Snapshot the ArtifactMap so the helper can resolve
                // source-crate handles to their artifact paths without
                // holding the lock across the filesystem copy. Pattern
                // mirrors the `Crate` arm above.
                let artifacts_snapshot =
                    lock_or_poison(self.state, "state")?.artifacts.clone();
                let (path, was_cached) = crate::generated::esp::ensure_esp(
                    self.ctx,
                    self.model,
                    self.resolved,
                    esp_handle,
                    &artifacts_snapshot,
                    stdout,
                )?;
                lock_or_poison(self.state, "state")?
                    .esps
                    .insert(esp_handle, path);
                self.record(was_cached);
                let _ = stderr;
                Ok(())
            }

            // ------------------------------------------------------------------
            // Image — assemble a bootable disk image. See image::build_image.
            // ------------------------------------------------------------------
            DagNode::Image(image_handle) => {
                // Snapshot artifacts and esp_dirs so the helper can
                // resolve sources without holding the locks across the
                // FAT format + write loop (which can take 100s of ms
                // for large images). One lock acquisition reads both
                // since they live in the same `ArtifactsSnapshot`.
                let (artifacts_snapshot, esp_dirs_snapshot) = {
                    let guard = lock_or_poison(self.state, "state")?;
                    (guard.artifacts.clone(), guard.esps.clone())
                };
                let ictx = ImageCtx {
                    ctx: self.ctx,
                    model: self.model,
                    resolved: self.resolved,
                    artifacts: &artifacts_snapshot,
                    esp_dirs: &esp_dirs_snapshot,
                };
                let (path, was_cached) = build_image(image_handle, &ictx, stdout)?;
                lock_or_poison(self.state, "state")?
                    .images
                    .insert(image_handle, path);
                self.record(was_cached);
                let _ = stderr;
                Ok(())
            }

            // ------------------------------------------------------------------
            // Rule — run a user-declared rule through the registry.
            // ------------------------------------------------------------------
            DagNode::Rule(rule_handle) => {
                // Rule nodes are not cacheable in MVP-M — no counter update.
                let rule = self.model.rules.get(rule_handle).ok_or_else(|| {
                    Error::Compile(format!(
                        "scheduler: Rule node references handle {:?} not in model.rules",
                        rule_handle
                    ))
                })?;
                // Snapshot artifacts (for `${artifact:<crate>}`) and
                // images (for `${image:<name>}`) so substitution sees a
                // stable view without holding the lock across the
                // rule's runtime. One lock acquisition for both.
                let (artifacts_snapshot, images_snapshot) = {
                    let guard = lock_or_poison(self.state, "state")?;
                    (guard.artifacts.clone(), guard.images.clone())
                };
                let rule_ctx = RuleCtx {
                    layout: &self.ctx.layout,
                    resolved: self.resolved,
                    model: self.model,
                    artifacts: &artifacts_snapshot,
                    image_paths: &images_snapshot,
                };
                self.rules.dispatch(&rule_ctx, rule, stdout, stderr)?;
                Ok(())
            }

            // ------------------------------------------------------------------
            // BuildStep — invoke a host-bin runner with curated env vars.
            // ------------------------------------------------------------------
            DagNode::BuildStep(handle) => {
                // Snapshot the artifact map so the runner sees the
                // runner crate's output and any crate_in inputs as
                // already-produced paths. Same lock-and-clone pattern
                // used by Crate / Rule / Image / Esp dispatch.
                let artifacts_snapshot =
                    lock_or_poison(self.state, "state")?.artifacts.clone();
                let (out_path, was_cached) = crate::compile::build_step::execute_build_step(
                    self.ctx,
                    self.model,
                    self.resolved,
                    handle,
                    &artifacts_snapshot,
                    stdout,
                    stderr,
                )?;
                lock_or_poison(self.state, "state")?
                    .artifacts
                    .set_build_step(handle, out_path);
                self.record(was_cached);
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
pub fn execute_pipeline(
    ctx: &CompileCtx,
    model: &BuildModel,
    resolved: &ResolvedConfig,
    rules: &RuleRegistry,
    workers: usize,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<BuildSummary> {
    let dag = build_dag(resolved, model)?;

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
        let result = execute_pipeline(&ctx, &model, &resolved, &rules, 1, &mut stdout, &mut stderr);

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
            execute_pipeline(&ctx, &model, &resolved, &rules, 1, &mut out, &mut err)
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
            execute_pipeline(&ctx, &model, &resolved, &rules, 1, &mut out, &mut err)
                .expect("second run should succeed");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_secs() < 2,
            "second run took too long ({elapsed:?}); cache hit expected"
        );
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
