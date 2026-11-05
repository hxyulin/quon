//! Rule registry and dispatch for user-defined build rules.
//!
//! A [`RuleRegistry`] holds named [`RuleFn`] implementations. During pipeline
//! execution (Chunk B4), the scheduler calls [`RuleRegistry::dispatch`] for
//! each rule node as it becomes ready. This module is self-contained: it does
//! not yet integrate with the scheduler; Chunk B4 wires that together.
//!
//! ### Design notes
//!
//! **Why `BTreeMap`?** Deterministic iteration order. While handler lookup by
//! name is not order-sensitive, future diagnostics that enumerate registered
//! rules (e.g. "did you mean ...?") must produce stable output. A `HashMap`
//! would produce arbitrarily ordered lists across runs.
//!
//! **Why separate `inputs` and `outputs` slices in [`RuleFn::execute`]?**
//! Merging them into one `Vec` forces each handler to track where inputs end
//! and outputs begin. Keeping them separate makes the contract explicit: the
//! built-in `exec` rule treats inputs as the command + argv and ignores
//! outputs (which the scheduler uses for cache-invalidation bookkeeping).

use crate::compile::BuildLayout;
use crate::compile::extern_map::ArtifactLookup;
use crate::error::{Error, Result};
use gluon_model::{BuildModel, ResolvedConfig, RuleDef, RuleHandler};
use std::collections::BTreeMap;

pub mod builtin;
mod substitution;

/// Execution context handed to a [`RuleFn`] when the scheduler invokes a rule.
///
/// Deliberately narrow: rules should not reach into the build cache directly
/// (the scheduler handles rule-level caching at the DAG level). Extend this
/// struct when a new capability is genuinely needed by an in-tree rule.
///
/// `model` is required because [`ResolvedConfig`] holds target handles but
/// not the target names themselves — we need the model to dereference
/// `resolved.profile.target` for `${target}` substitution.
pub struct RuleCtx<'a> {
    pub layout: &'a BuildLayout,
    pub resolved: &'a ResolvedConfig,
    pub model: &'a BuildModel,
    /// Outputs of finished nodes. `${image:<name>}` substitution reads
    /// image paths from here; an image that has not been built yet
    /// gives a clear "image not built yet" diagnostic.
    pub artifacts: &'a dyn ArtifactLookup,
}

/// A rule handler. Implementations receive pre-substituted argument strings —
/// the registry performs `${var}` expansion before dispatch.
///
/// ### Contract
///
/// - `inputs`: the substituted input strings. For the built-in `exec` rule the
///   first element is the command path and the remainder are its argv.
/// - `outputs`: the substituted output strings. The scheduler uses this list
///   for cache-invalidation bookkeeping. Most handlers can ignore it.
///
/// The two slices are passed separately (rather than concatenated) so handlers
/// can unambiguously distinguish between command arguments and output paths
/// without needing an out-of-band length.
pub trait RuleFn: Send + Sync {
    fn execute(
        &self,
        ctx: &RuleCtx<'_>,
        rule: &RuleDef,
        inputs: &[String],
        outputs: &[String],
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> Result<()>;
}

/// Registry of rule handlers keyed by name.
///
/// Use [`RuleRegistry::with_builtins`] for normal operation; use
/// [`RuleRegistry::new`] in tests when you want a clean slate.
pub struct RuleRegistry {
    // BTreeMap for deterministic iteration — see module-level note.
    handlers: BTreeMap<String, Box<dyn RuleFn>>,
}

impl Default for RuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleRegistry {
    /// Empty registry with no handlers.
    ///
    /// Use for tests or when the caller wants to hand-pick which builtins to
    /// register.
    pub fn new() -> Self {
        Self {
            handlers: BTreeMap::new(),
        }
    }

    /// Registry pre-populated with every in-tree builtin.
    ///
    /// MVP-M registers only `exec`; future chunks may add more.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register("exec", Box::new(builtin::ExecRule));
        r
    }

    /// Register a handler under the given name, overwriting any previous one.
    pub fn register(&mut self, name: impl Into<String>, handler: Box<dyn RuleFn>) {
        self.handlers.insert(name.into(), handler);
    }

    /// Returns `true` if a handler with this name has been registered.
    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    /// Dispatch a rule: validate the handler kind, substitute `${var}` tokens,
    /// and call the selected handler.
    ///
    /// ### Error cases
    ///
    /// - `rule.handler` is `RuleHandler::Script(_)` — script-backed rules are
    ///   removed; returns a contextual `Error::Compile`.
    /// - `rule.handler` is `RuleHandler::Builtin(name)` and `name` is not
    ///   registered — the error message names the rule, the requested builtin,
    ///   and the sorted list of registered builtin names.
    /// - Substitution encounters an unknown `${var}` — message names the rule
    ///   and the unknown variable.
    /// - Substitution encounters an unterminated `${...}` — message names the
    ///   rule and the offending token.
    pub fn dispatch(
        &self,
        ctx: &RuleCtx<'_>,
        rule: &RuleDef,
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> Result<()> {
        // Script-backed rules were a Rhai-era feature; no public path
        // constructs RuleHandler::Script(_) any more (the gluon-config
        // builder API exposes no `.on_execute(...)`). Defending against
        // a stale model JSON that still carries the variant is cheap.
        let builtin_name = match &rule.handler {
            RuleHandler::Script(_) => {
                return Err(Error::Compile(format!(
                    "rule '{}': script-backed handlers are no longer \
                     supported; use a builtin handler",
                    rule.name
                )));
            }
            RuleHandler::Builtin(name) => name,
        };

        let handler = self.handlers.get(builtin_name.as_str()).ok_or_else(|| {
            let available: Vec<&str> = self.handlers.keys().map(|s| s.as_str()).collect();
            Error::Compile(format!(
                "rule '{}': unknown builtin '{}'; registered builtins: [{}]",
                rule.name,
                builtin_name,
                available.join(", ")
            ))
        })?;

        // Substitute ${var} tokens in inputs and outputs separately so the
        // handler can tell them apart (see RuleFn doc comment).
        let s = substitution::Substituter::new(ctx, rule);
        let inputs = s.expand_args(&rule.inputs)?;
        let outputs = s.expand_args(&rule.outputs)?;

        handler.execute(ctx, rule, &inputs, &outputs, stdout, stderr)
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

/// Shared test fixtures for the `rule` module tree. Lifted to a top-level
/// `#[cfg(test)] pub(crate)` sibling so `mod.rs`, `builtin.rs`, and
/// `substitution.rs` tests all reach it via `crate::rule::test_support::*`.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::compile::{ArtifactMap, BuildLayout};
    use gluon_model::{BuildModel, Handle, ProjectDef, ResolvedConfig, ResolvedProfile, TargetDef};
    use std::path::PathBuf;

    pub fn make_target(name: &str) -> TargetDef {
        TargetDef {
            name: name.into(),
            spec: format!("{}-unknown-none", name),
            builtin: true,
            panic_strategy: None,
            span: None,
        }
    }

    pub fn make_model_with_target(target_name: &str) -> (BuildModel, Handle<TargetDef>) {
        let mut model = BuildModel::default();
        let t = make_target(target_name);
        let (handle, _) = model.targets.insert(target_name.into(), t);
        (model, handle)
    }

    pub fn make_profile(target: Handle<TargetDef>) -> ResolvedProfile {
        ResolvedProfile {
            name: "debug".into(),
            target,
            opt_level: 0,
            debug_info: false,
            lto: None,
            boot_binary: None,
            qemu_memory: None,
            qemu_cores: None,
            qemu_extra_args: Vec::new(),
            qemu_boot_mode: None,
            test_timeout: None,
        }
    }

    pub fn make_resolved(target_handle: Handle<TargetDef>, build_dir: PathBuf) -> ResolvedConfig {
        ResolvedConfig {
            project: ProjectDef {
                name: "testproject".into(),
                version: "0.1.0".into(),
                config_crate_name: None,
                cfg_prefix: None,
                config_override_file: None,
                default_profile: None,
            },
            profile: make_profile(target_handle),
            options: Default::default(),
            crates: Vec::new(),
            build_dir: build_dir.clone(),
            project_root: build_dir.parent().unwrap_or(&build_dir).to_path_buf(),
        }
    }

    /// Build a `(BuildLayout, BuildModel, ResolvedConfig, ArtifactMap)` tuple
    /// for testing.
    ///
    /// `tmp` should be a `tempfile::TempDir` base; the build dir is
    /// `tmp/build`.
    pub fn make_ctx_parts(
        tmp: &std::path::Path,
    ) -> (BuildLayout, BuildModel, ResolvedConfig, ArtifactMap) {
        let build_dir = tmp.join("build");
        std::fs::create_dir_all(&build_dir).unwrap();
        let (model, target_handle) = make_model_with_target("x86_64-test");
        let resolved = make_resolved(target_handle, build_dir.clone());
        let layout = BuildLayout::new(build_dir, "testproject");
        let artifacts = ArtifactMap::new();
        (layout, model, resolved, artifacts)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::test_support::*;

    // -----------------------------------------------------------------------
    // Registry tests
    // -----------------------------------------------------------------------

    #[test]
    fn empty_registry_has_no_handlers() {
        assert!(!RuleRegistry::new().contains("exec"));
    }

    #[test]
    fn with_builtins_registers_exec() {
        assert!(RuleRegistry::with_builtins().contains("exec"));
    }

    #[test]
    fn dispatch_unknown_builtin_returns_compile_error_with_available_list() {
        // A capturing dummy handler so we have at least one registered name.
        struct FakeRule;
        impl RuleFn for FakeRule {
            fn execute(
                &self,
                _ctx: &RuleCtx<'_>,
                _rule: &RuleDef,
                _inputs: &[String],
                _outputs: &[String],
                _stdout: &mut Vec<u8>,
                _stderr: &mut Vec<u8>,
            ) -> Result<()> {
                Ok(())
            }
        }

        let mut registry = RuleRegistry::new();
        registry.register("fake", Box::new(FakeRule));

        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let rule = RuleDef {
            name: "myrule".into(),
            inputs: vec![],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("nope".into()),
            working_dir: None,
            span: None,
        };

        let err = registry
            .dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(
                    msg.contains("nope"),
                    "error should mention the unknown builtin: {msg}"
                );
                assert!(
                    msg.contains("fake"),
                    "error should list registered builtins: {msg}"
                );
                assert!(msg.contains("myrule"), "error should name the rule: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_script_handler_returns_removed_error() {
        let registry = RuleRegistry::new();
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };
        let rule = RuleDef {
            name: "scriptrule".into(),
            inputs: vec![],
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Script("any".into()),
            working_dir: None,
            span: None,
        };

        let err = registry
            .dispatch(&ctx, &rule, &mut Vec::new(), &mut Vec::new())
            .unwrap_err();
        match err {
            Error::Compile(msg) => {
                assert!(msg.contains("scriptrule"), "should name rule: {msg}");
                assert!(
                    msg.contains("no longer supported"),
                    "should explain removal: {msg}"
                );
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    // Substitution tests live in rule/substitution.rs.
}
