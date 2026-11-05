//! `Substituter` — argument-string expansion for rule invocations.
//!
//! Holds a borrowed `(RuleCtx, RuleDef)` pair so call sites stop
//! threading three arguments through every layer. Five entry
//! points:
//!
//! - `expand_args` — top-level: maps `expand_one` over a slice.
//! - `expand_one` — scans a single string for `${...}` tokens and
//!   replaces them.
//! - `resolve_var` — handles `${var_name}` lookups, dispatching
//!   `${artifact:...}` / `${image:...}` to the resolvers below.
//! - `resolve_artifact` — `${artifact:<crate>}` lookup.
//! - `resolve_image` — `${image:<name>}` lookup.
//!
//! See the Stage 7 plan's "Decisions baked in" §7 for why this is
//! a struct, not a trait.

use crate::error::{Error, Result};
use crate::rule::{RuleCtx, RuleDef};
use gluon_model::{CrateDef, TargetDef};

/// Simple variable names recognised by [`Substituter::resolve_var`]. Sorted
/// so the diagnostic in the unknown-variable error is deterministic. Prefixed
/// forms (`${artifact:<crate>}`, `${image:<name>}`) are handled separately
/// and are intentionally not in this list.
const KNOWN_VARS: &[&str] = &[
    "build_dir",
    "profile",
    "project_name",
    "project_root",
    "target",
];

/// Argument-string expander bound to a `(RuleCtx, RuleDef)` pair.
///
/// Constructed once per rule invocation, then reused for both
/// `inputs` and `outputs` expansion so the borrow pair is plumbed
/// in one place rather than threaded through every call.
pub(crate) struct Substituter<'a> {
    ctx: &'a RuleCtx<'a>,
    rule: &'a RuleDef,
}

impl<'a> Substituter<'a> {
    pub(crate) fn new(ctx: &'a RuleCtx<'a>, rule: &'a RuleDef) -> Self {
        Self { ctx, rule }
    }

    /// Expand `${...}` tokens in every arg.
    pub(crate) fn expand_args(&self, args: &[String]) -> Result<Vec<String>> {
        args.iter().map(|a| self.expand_one(a)).collect()
    }

    /// Expand `${...}` tokens in a single string.
    pub(crate) fn expand_one(&self, s: &str) -> Result<String> {
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
                                self.rule.name, s
                            ))
                        })?;

                    let var_name = &s[var_start..var_end];
                    let replacement = self.resolve_var(var_name)?;
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

    /// Resolve a single `${var_name}` token.
    pub(crate) fn resolve_var(&self, var_name: &str) -> Result<String> {
        // Prefixed variables: ${artifact:<crate_name>}
        if let Some(crate_name) = var_name.strip_prefix("artifact:") {
            return self.resolve_artifact(crate_name);
        }
        // Prefixed variables: ${image:<image_name>}
        if let Some(image_name) = var_name.strip_prefix("image:") {
            return self.resolve_image(image_name);
        }

        match var_name {
            "build_dir" => Ok(self.ctx.layout.root().display().to_string()),
            "project_name" => Ok(self.ctx.resolved.project.name.clone()),
            "project_root" => Ok(self.ctx.resolved.project_root.display().to_string()),
            "profile" => Ok(self.ctx.resolved.profile.name.clone()),
            "target" => {
                let target: &TargetDef = self
                    .ctx
                    .model
                    .targets
                    .get(self.ctx.resolved.profile.target)
                    .ok_or_else(|| {
                        Error::Compile(format!(
                            "rule '{}': internal error — target handle {:?} not found in \
                                 build model (this is a bug in gluon, not in your configuration)",
                            self.rule.name, self.ctx.resolved.profile.target
                        ))
                    })?;
                Ok(target.name.clone())
            }
            unknown => Err(Error::Compile(format!(
                "rule '{}': unknown substitution variable '${{{}}}'; \
                 known variables: [{}], or use ${{artifact:<crate_name>}} / \
                 ${{image:<image_name>}}",
                self.rule.name,
                unknown,
                KNOWN_VARS.join(", ")
            ))),
        }
    }

    /// Resolve `${image:<image_name>}`.
    pub(crate) fn resolve_image(&self, image_name: &str) -> Result<String> {
        if image_name.is_empty() {
            return Err(Error::Compile(format!(
                "rule '{}': empty image name in ${{image:}}",
                self.rule.name
            )));
        }

        let handle = self.ctx.model.images.lookup(image_name).ok_or_else(|| {
            let available: Vec<&str> = self
                .ctx
                .model
                .images
                .iter()
                .map(|(_, i): (_, &gluon_model::ImageDef)| i.name.as_str())
                .collect();
            Error::Compile(format!(
                "rule '{}': ${{image:{}}} references unknown image; available: [{}]",
                self.rule.name,
                image_name,
                available.join(", ")
            ))
        })?;

        let path = self.ctx.artifacts.image(handle).ok_or_else(|| {
            Error::Compile(format!(
                "rule '{}': ${{image:{}}} — image is declared but no output path is \
                 registered yet (ensure the rule's pipeline stage runs after the image \
                 scheduler node)",
                self.rule.name, image_name
            ))
        })?;

        Ok(path.display().to_string())
    }

    /// Resolve `${artifact:<crate_name>}`.
    pub(crate) fn resolve_artifact(&self, crate_name: &str) -> Result<String> {
        if crate_name.is_empty() {
            return Err(Error::Compile(format!(
                "rule '{}': empty crate name in ${{artifact:}}",
                self.rule.name
            )));
        }

        let handle = self.ctx.model.crates.lookup(crate_name).ok_or_else(|| {
            let available: Vec<&str> = self
                .ctx
                .model
                .crates
                .iter()
                .map(|(_, c): (_, &CrateDef)| c.name.as_str())
                .collect();
            Error::Compile(format!(
                "rule '{}': ${{artifact:{}}} references unknown crate; \
                 available crates: [{}]",
                self.rule.name,
                crate_name,
                available.join(", ")
            ))
        })?;

        let path = self.ctx.artifacts.get(handle).ok_or_else(|| {
            Error::Compile(format!(
                "rule '{}': ${{artifact:{}}} — crate exists but has no compiled \
                 artifact (ensure the rule's pipeline stage depends on the crate's group)",
                self.rule.name, crate_name
            ))
        })?;

        Ok(path.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::RuleCtx;
    use crate::rule::test_support::*;
    use gluon_model::{RuleDef, RuleHandler};

    fn make_rule(name: &str, inputs: Vec<String>) -> RuleDef {
        RuleDef {
            name: name.into(),
            inputs,
            outputs: vec![],
            depends_on: vec![],
            handler: RuleHandler::Builtin("capture".into()),
            working_dir: None,
            span: None,
        }
    }

    // -----------------------------------------------------------------------
    // Substitution tests
    // -----------------------------------------------------------------------

    #[test]
    fn substitution_expands_args() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let cases: Vec<(&str, &str, String)> = vec![
            (
                "build_dir",
                "${build_dir}/foo",
                format!("{}/foo", tmp.path().join("build").display()),
            ),
            ("project_name", "${project_name}", "testproject".into()),
            ("profile", "${profile}", "debug".into()),
            ("target", "${target}", "x86_64-test".into()),
            // A broken loop-advance after the first token would duplicate
            // or drop bytes around the later ones.
            (
                "several vars in one arg",
                "${project_name}-${profile}-${target}",
                "testproject-debug-x86_64-test".into(),
            ),
            // Earlier drafts used `bytes[i] as char`, which split
            // multi-byte UTF-8 sequences.
            (
                "non-ASCII literal bytes",
                "café-${profile}-naïve",
                "café-debug-naïve".into(),
            ),
            (
                "literal $ without brace",
                "$HOME/no_brace",
                "$HOME/no_brace".into(),
            ),
            (
                "project_root",
                "${project_root}/keys/sign.key",
                format!("{}/keys/sign.key", resolved.project_root.display()),
            ),
            ("$$ before brace", "echo $${HOME}", "echo ${HOME}".into()),
            ("$$ standalone", "cost: $$5", "cost: $5".into()),
        ];

        // Expand every input in one call so the test also pins that
        // `expand_args` keeps one output per input, in order.
        let rule = make_rule(
            "testrule",
            cases
                .iter()
                .map(|(_, input, _)| input.to_string())
                .collect(),
        );
        let args = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap();
        assert_eq!(args.len(), cases.len());
        for ((name, _, expected), got) in cases.iter().zip(&args) {
            assert_eq!(got, expected, "{name}");
        }
    }

    #[test]
    fn substitution_unknown_var_errors_with_rule_name_and_var_name() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("badrule", vec!["${nope}".into()]);

        let err = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap_err();
        match err {
            crate::error::Error::Compile(msg) => {
                assert!(msg.contains("badrule"), "should name rule: {msg}");
                assert!(msg.contains("nope"), "should name unknown var: {msg}");
                // Should also mention the list of known vars.
                assert!(msg.contains("build_dir"), "should list known vars: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    #[test]
    fn substitution_unterminated_brace_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("badrule", vec!["${unterminated".into()]);

        let err = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap_err();
        match err {
            crate::error::Error::Compile(msg) => {
                assert!(msg.contains("badrule"), "should name rule: {msg}");
                // The message should hint about the unterminated sequence.
                assert!(
                    msg.contains("unterminated") || msg.contains("${"),
                    "should hint at unterminated sequence: {msg}"
                );
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Artifact substitution tests
    // -----------------------------------------------------------------------

    #[test]
    fn substitution_resolves_artifact_by_crate_name() {
        use gluon_model::{CrateDef, GroupDef, TargetSpec};

        let tmp = tempfile::tempdir().unwrap();
        let (layout, mut model, resolved, mut artifacts) = make_ctx_parts(tmp.path());

        // Register a crate in the model and record its artifact.
        let target_h = model.targets.lookup("x86_64-test").expect("target");
        let (group_h, _) = model.groups.insert(
            "kernel_group".into(),
            GroupDef {
                name: "kernel_group".into(),
                target: TargetSpec::Cross(target_h),
                ..Default::default()
            },
        );
        let crate_def = CrateDef {
            name: "kernel".into(),
            path: "crates/kernel".into(),
            edition: "2021".into(),
            crate_type: gluon_model::CrateType::Bin,
            target: TargetSpec::Cross(target_h),
            deps: Default::default(),
            dev_deps: Default::default(),
            features: vec![],
            root: None,
            linker_script: None,
            group: group_h,
            is_project_crate: true,
            cfg_flags: vec![],
            rustc_flags: vec![],
            requires_config: vec![],
            artifact_deps: vec![],
            artifact_env: Default::default(),
            span: None,
            cargo: None,
        };
        let (handle, _) = model.crates.insert("kernel".into(), crate_def);
        let artifact_path = tmp.path().join("build/cross/x86_64-test/debug/kernel");
        artifacts.insert(handle, artifact_path.clone());

        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("strip", vec!["objcopy".into(), "${artifact:kernel}".into()]);

        let args = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap();
        assert_eq!(args[1], artifact_path.display().to_string());
    }

    #[test]
    fn substitution_artifact_unknown_crate_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("badrule", vec!["${artifact:nonexistent}".into()]);

        let err = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap_err();
        match err {
            crate::error::Error::Compile(msg) => {
                assert!(msg.contains("nonexistent"), "should name crate: {msg}");
                assert!(msg.contains("badrule"), "should name rule: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Image substitution tests
    // -----------------------------------------------------------------------
    //
    // ${image:name} mirrors ${artifact:crate} but resolves through
    // `ArtifactLookup::image` instead of a crate artifact.
    // The shape parallels the artifact tests above.

    #[test]
    fn substitution_resolves_image_by_name() {
        use gluon_model::ImageDef;

        let tmp = tempfile::tempdir().unwrap();
        let (layout, mut model, resolved, mut artifacts) = make_ctx_parts(tmp.path());

        let (image_h, _) = model.images.insert(
            "boot-disk".into(),
            ImageDef {
                name: "boot-disk".into(),
                ..Default::default()
            },
        );
        let image_path = tmp.path().join("build/images/boot-disk.img");
        artifacts.set_image(image_h, image_path.clone());

        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule(
            "checksum",
            vec!["sha256sum".into(), "${image:boot-disk}".into()],
        );

        let args = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap();
        assert_eq!(args[1], image_path.display().to_string());
    }

    #[test]
    fn substitution_image_unknown_name_lists_available() {
        use gluon_model::ImageDef;

        let tmp = tempfile::tempdir().unwrap();
        let (layout, mut model, resolved, artifacts) = make_ctx_parts(tmp.path());

        model.images.insert(
            "alpha".into(),
            ImageDef {
                name: "alpha".into(),
                ..Default::default()
            },
        );
        model.images.insert(
            "beta".into(),
            ImageDef {
                name: "beta".into(),
                ..Default::default()
            },
        );

        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("checksum", vec!["${image:gamma}".into()]);

        let err = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gamma"), "{msg}");
        assert!(msg.contains("alpha") && msg.contains("beta"), "{msg}");
    }

    #[test]
    fn substitution_image_known_but_no_path_diagnoses_ordering() {
        use gluon_model::ImageDef;

        let tmp = tempfile::tempdir().unwrap();
        let (layout, mut model, resolved, artifacts) = make_ctx_parts(tmp.path());
        model.images.insert(
            "boot-disk".into(),
            ImageDef {
                name: "boot-disk".into(),
                ..Default::default()
            },
        );

        // No image is recorded: simulates a rule that fires before the
        // image scheduler node has produced any output.
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("checksum", vec!["${image:boot-disk}".into()]);

        let err = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("scheduler") || msg.contains("pipeline stage"),
            "diagnostic should hint at the ordering bug, got: {msg}"
        );
    }

    #[test]
    fn substitution_image_empty_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("badrule", vec!["${image:}".into()]);

        let err = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty"), "{msg}");
    }

    #[test]
    fn substitution_artifact_empty_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (layout, model, resolved, artifacts) = make_ctx_parts(tmp.path());
        let ctx = RuleCtx {
            layout: &layout,
            resolved: &resolved,
            model: &model,
            artifacts: &artifacts,
        };

        let rule = make_rule("badrule", vec!["${artifact:}".into()]);

        let err = Substituter::new(&ctx, &rule)
            .expand_args(&rule.inputs)
            .unwrap_err();
        match err {
            crate::error::Error::Compile(msg) => {
                assert!(msg.contains("empty"), "should mention empty: {msg}");
            }
            other => panic!("expected Error::Compile, got {other:?}"),
        }
    }
}
