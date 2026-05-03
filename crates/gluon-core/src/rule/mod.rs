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

use crate::compile::{ArtifactMap, BuildLayout};
use crate::error::{Error, Result};
use gluon_model::{BuildModel, CrateDef, ResolvedConfig, RuleDef, RuleHandler, TargetDef};
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
    pub artifacts: &'a ArtifactMap,
    /// Output paths for built images, keyed by [`gluon_model::ImageDef`]
    /// handle. Used by `${image:<name>}` substitution. May be empty
    /// when no `image()` blocks were declared, or when the rule fires
    /// before the scheduler has produced any image artifacts (the
    /// substitution will then surface a clear "image not built yet"
    /// diagnostic).
    pub image_paths: &'a std::collections::BTreeMap<
        gluon_model::Handle<gluon_model::ImageDef>,
        std::path::PathBuf,
    >,
}

impl<'a> RuleCtx<'a> {
    /// Static reference to an empty image-path map. Useful for tests
    /// and call sites that legitimately have no image artifacts to
    /// expose (e.g. the `gluon vendor` workflow). Avoids forcing every
    /// caller to thread a local `BTreeMap::new()` through the scope.
    pub fn empty_image_paths() -> &'static std::collections::BTreeMap<
        gluon_model::Handle<gluon_model::ImageDef>,
        std::path::PathBuf,
    > {
        use std::sync::OnceLock;
        static MAP: OnceLock<
            std::collections::BTreeMap<
                gluon_model::Handle<gluon_model::ImageDef>,
                std::path::PathBuf,
            >,
        > = OnceLock::new();
        MAP.get_or_init(std::collections::BTreeMap::new)
    }
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
        let inputs = substitute_args(ctx, rule, &rule.inputs)?;
        let outputs = substitute_args(ctx, rule, &rule.outputs)?;

        handler.execute(ctx, rule, &inputs, &outputs, stdout, stderr)
    }
}

// ---------------------------------------------------------------------------
// ${var} substitution
// ---------------------------------------------------------------------------

/// The set of simple variable names recognised by the substitution engine.
/// Kept as a sorted slice so error messages are deterministic.
///
/// In addition to these, the prefixed form `${artifact:<crate_name>}` resolves
/// to the output path of a compiled crate.
const KNOWN_VARS: &[&str] = &[
    "build_dir",
    "profile",
    "project_name",
    "project_root",
    "target",
];

/// Substitute all `${var}` tokens in a list of argument strings.
///
/// Returns a fresh `Vec<String>` — one element per input string — with all
/// recognised `${var}` tokens replaced. Unknown variables and unterminated
/// `${...}` sequences are reported as `Error::Compile` naming the offending
/// rule.
fn substitute_args(ctx: &RuleCtx<'_>, rule: &RuleDef, args: &[String]) -> Result<Vec<String>> {
    args.iter()
        .map(|arg| substitute_one(ctx, rule, arg))
        .collect()
}

/// Substitute `${var}` tokens in a single string.
///
/// ### Substitution behaviour
///
/// - `$$` produces a literal `$` (escape mechanism).
/// - `$${foo}` produces a literal `${foo}`.
/// - A bare `$` not followed by `$` or `{` is emitted literally.
/// - `${build_dir}` → the build output directory
/// - `${project_name}` → the project name from config
/// - `${project_root}` → the project root directory
/// - `${profile}` → the active profile name
/// - `${target}` → the target triple name
/// - `${artifact:<name>}` → the compiled output path for the named crate
fn substitute_one(ctx: &RuleCtx<'_>, rule: &RuleDef, s: &str) -> Result<String> {
    // Fast path: no substitution needed.
    if !s.contains('$') {
        return Ok(s.to_owned());
    }

    let mut out = String::with_capacity(s.len() + 32);
    let bytes = s.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$' {
            // `$$` → literal `$` (escape).
            if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
            } else if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                // Opening `${` found — scan forward for `}`.
                let var_start = i + 2; // first byte of variable name
                let var_end = bytes[var_start..]
                    .iter()
                    .position(|&b| b == b'}')
                    .map(|pos| var_start + pos)
                    .ok_or_else(|| {
                        Error::Compile(format!(
                            "rule '{}': unterminated '${{...}}' sequence in argument: '{}'",
                            rule.name, s
                        ))
                    })?;

                let var_name = &s[var_start..var_end];
                let replacement = resolve_var(ctx, rule, var_name)?;
                out.push_str(&replacement);
                // Advance past the closing `}`.
                i = var_end + 1;
            } else {
                // Bare `$` not followed by `{` — emit literally.
                out.push('$');
                i += 1;
            }
        } else {
            // Decode the next UTF-8 codepoint at `i`. A naive `bytes[i] as
            // char` cast would silently split multi-byte characters (e.g.
            // 'é' = [0xC3, 0xA9]) into two invalid chars. `{` / `}` / `$`
            // are all ASCII so the brace-scanning branches above are safe
            // on byte indices; only this literal-copy branch needs to
            // advance by a full codepoint's worth of bytes.
            let ch = s[i..]
                .chars()
                .next()
                .expect("index is within &str bounds so at least one char follows");
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    Ok(out)
}

/// Resolve a single variable name to its string value.
///
/// Handles simple variables (`build_dir`, `profile`, etc.) and the prefixed
/// form `artifact:<crate_name>` which resolves to a compiled artifact path.
///
/// Returns `Error::Compile` for unknown variables, naming the rule and
/// listing known variables for discoverability.
fn resolve_var(ctx: &RuleCtx<'_>, rule: &RuleDef, var_name: &str) -> Result<String> {
    // Prefixed variables: ${artifact:<crate_name>}
    if let Some(crate_name) = var_name.strip_prefix("artifact:") {
        return resolve_artifact(ctx, rule, crate_name);
    }
    // Prefixed variables: ${image:<image_name>}
    if let Some(image_name) = var_name.strip_prefix("image:") {
        return resolve_image(ctx, rule, image_name);
    }

    match var_name {
        "build_dir" => Ok(ctx.layout.root().display().to_string()),
        "project_name" => Ok(ctx.resolved.project.name.clone()),
        "project_root" => Ok(ctx.resolved.project_root.display().to_string()),
        "profile" => Ok(ctx.resolved.profile.name.clone()),
        "target" => {
            let target: &TargetDef = ctx
                .model
                .targets
                .get(ctx.resolved.profile.target)
                .ok_or_else(|| {
                    Error::Compile(format!(
                        "rule '{}': internal error — target handle {:?} not found in \
                             build model (this is a bug in gluon, not in your configuration)",
                        rule.name, ctx.resolved.profile.target
                    ))
                })?;
            Ok(target.name.clone())
        }
        unknown => Err(Error::Compile(format!(
            "rule '{}': unknown substitution variable '${{{}}}'; \
             known variables: [{}], or use ${{artifact:<crate_name>}} / \
             ${{image:<image_name>}}",
            rule.name,
            unknown,
            KNOWN_VARS.join(", ")
        ))),
    }
}

/// Resolve `${image:<image_name>}` to the built image's output path.
///
/// Mirrors [`resolve_artifact`]: validates the name resolves in the
/// model first (so an unknown image name produces a discoverable error
/// listing the available images), then looks up the actual output path
/// in the scheduler's `image_paths` snapshot.
fn resolve_image(ctx: &RuleCtx<'_>, rule: &RuleDef, image_name: &str) -> Result<String> {
    if image_name.is_empty() {
        return Err(Error::Compile(format!(
            "rule '{}': empty image name in ${{image:}}",
            rule.name
        )));
    }

    let handle = ctx.model.images.lookup(image_name).ok_or_else(|| {
        let available: Vec<&str> = ctx
            .model
            .images
            .iter()
            .map(|(_, i): (_, &gluon_model::ImageDef)| i.name.as_str())
            .collect();
        Error::Compile(format!(
            "rule '{}': ${{image:{}}} references unknown image; available: [{}]",
            rule.name,
            image_name,
            available.join(", ")
        ))
    })?;

    let path = ctx.image_paths.get(&handle).ok_or_else(|| {
        Error::Compile(format!(
            "rule '{}': ${{image:{}}} — image is declared but no output path is \
             registered yet (ensure the rule's pipeline stage runs after the image \
             scheduler node)",
            rule.name, image_name
        ))
    })?;

    Ok(path.display().to_string())
}

/// Resolve `${artifact:<crate_name>}` to the compiled output path.
///
/// Looks up the crate by name in the build model, then retrieves its artifact
/// path from the snapshot. Returns clear errors if the crate doesn't exist or
/// hasn't been compiled yet.
fn resolve_artifact(ctx: &RuleCtx<'_>, rule: &RuleDef, crate_name: &str) -> Result<String> {
    if crate_name.is_empty() {
        return Err(Error::Compile(format!(
            "rule '{}': empty crate name in ${{artifact:}}",
            rule.name
        )));
    }

    let handle = ctx.model.crates.lookup(crate_name).ok_or_else(|| {
        let available: Vec<&str> = ctx
            .model
            .crates
            .iter()
            .map(|(_, c): (_, &CrateDef)| c.name.as_str())
            .collect();
        Error::Compile(format!(
            "rule '{}': ${{artifact:{}}} references unknown crate; \
             available crates: [{}]",
            rule.name,
            crate_name,
            available.join(", ")
        ))
    })?;

    let path = ctx.artifacts.get(handle).ok_or_else(|| {
        Error::Compile(format!(
            "rule '{}': ${{artifact:{}}} — crate exists but has no compiled \
             artifact (ensure the rule's pipeline stage depends on the crate's group)",
            rule.name, crate_name
        ))
    })?;

    Ok(path.display().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::{BuildModel, Handle, ProjectDef, TargetDef};
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Test fixtures
    // -----------------------------------------------------------------------

    /// Shared helper module so builtin.rs tests can `use super::test_support::*`.
    pub(crate) mod test_support {
        use super::*;
        use crate::compile::{ArtifactMap, BuildLayout};
        use gluon_model::{ResolvedConfig, ResolvedProfile};

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

        pub fn make_resolved(
            target_handle: Handle<TargetDef>,
            build_dir: PathBuf,
        ) -> ResolvedConfig {
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

    use test_support::*;

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
            image_paths: RuleCtx::empty_image_paths(),
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
            image_paths: RuleCtx::empty_image_paths(),
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
