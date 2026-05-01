//! Boundary between `RawBuildModel` (gluon-config emits) and
//! `BuildModel` (gluon-core consumes).
//!
//! The actual cross-reference resolution is duplicated from
//! `gluon-core/src/engine/intern.rs` (Rhai path) and lives here to keep
//! gluon-config's emit → gluon-core's consume boundary serde-only.
//! Stage 3 deletes the Rhai engine and this becomes the sole intern.

use crate::build_model::{
    BuildModel, BuildStepDef, BuildStepInput, CrateDef, EspDef, GroupDef, PipelineDef, ProfileDef,
    TargetDef,
};
use crate::handle::Handle;
use crate::raw::RawBuildModel;
use crate::source::SourceSpan;

/// Diagnostic produced by `intern`. `gluon-core` translates these to its
/// own `Diagnostic` type at the `script_runner` boundary.
#[derive(Debug, Clone)]
pub struct InternDiagnostic {
    pub message: String,
    pub span: Option<SourceSpan>,
}

impl InternDiagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self { message: message.into(), span: None }
    }
    pub fn with_span(mut self, span: SourceSpan) -> Self {
        self.span = Some(span);
        self
    }
    pub fn with_optional_span(mut self, span: Option<SourceSpan>) -> Self {
        if let Some(s) = span {
            self.span = Some(s);
        }
        self
    }
}

/// Resolve every cross-reference in `raw` and produce a fully-populated
/// `BuildModel`. Dangling references accumulate into the returned
/// `Vec<InternDiagnostic>`; the model is still returned (with `None`
/// handles for the unresolved slots) so callers can choose whether to
/// surface partial state or fail.
pub fn intern(raw: RawBuildModel) -> (BuildModel, Vec<InternDiagnostic>) {
    let mut model = raw.model;
    let mut diags = Vec::new();
    intern_profiles(&mut model, &mut diags);
    intern_groups(&mut model, &mut diags);
    intern_crates(&mut model, &mut diags);
    intern_pipelines(&mut model, &mut diags);
    intern_esps(&mut model, &mut diags);
    intern_build_steps(&mut model, &mut diags);
    (model, diags)
}

/// Per-profile resolution result. Each inner `Option` records whether the
/// profile had a string ref in that slot at all; the inner `Option<Handle>`
/// is the resolved handle (`None` if the lookup failed — the diagnostic has
/// already been pushed in that case).
type ProfileResolution = (
    Handle<ProfileDef>,
    Option<Option<Handle<ProfileDef>>>,
    Option<Option<Handle<TargetDef>>>,
    Option<Option<Handle<CrateDef>>>,
);

fn intern_profiles(model: &mut BuildModel, diags: &mut Vec<InternDiagnostic>) {
    let mut updates: Vec<ProfileResolution> = Vec::with_capacity(model.profiles.len());

    for (handle, profile) in model.profiles.iter() {
        let inherits = profile.inherits.as_ref().map(|name| {
            let h = model.profiles.lookup(name);
            if h.is_none() {
                diags.push(
                    InternDiagnostic::error(format!(
                        "profile '{}' inherits from unknown profile '{}'",
                        profile.name, name
                    ))
                    .with_optional_span(profile.span.clone()),
                );
            }
            h
        });
        let target = profile.target.as_ref().map(|name| {
            let h = model.targets.lookup(name);
            if h.is_none() {
                diags.push(
                    InternDiagnostic::error(format!(
                        "profile '{}' references unknown target '{}'",
                        profile.name, name
                    ))
                    .with_optional_span(profile.span.clone()),
                );
            }
            h
        });
        let boot_binary = profile.boot_binary.as_ref().map(|name| {
            let h = model.crates.lookup(name);
            if h.is_none() {
                diags.push(
                    InternDiagnostic::error(format!(
                        "profile '{}' references unknown boot binary crate '{}'",
                        profile.name, name
                    ))
                    .with_optional_span(profile.span.clone()),
                );
            }
            h
        });
        updates.push((handle, inherits, target, boot_binary));
    }

    for (handle, inherits, target, boot_binary) in updates {
        if let Some(profile) = model.profiles.get_mut(handle) {
            if let Some(v) = inherits {
                profile.inherits_handle = v;
            }
            if let Some(v) = target {
                profile.target_handle = v;
            }
            if let Some(v) = boot_binary {
                profile.boot_binary_handle = v;
            }
        }
    }
}

fn intern_groups(model: &mut BuildModel, diags: &mut Vec<InternDiagnostic>) {
    // Groups always carry a mandatory `target: String`. The literal string
    // "host" is a sentinel meaning "use the host triple" and is not expected
    // to be in `model.targets`; leaving its handle `None` is correct.
    let mut updates: Vec<(Handle<GroupDef>, Option<Handle<TargetDef>>)> =
        Vec::with_capacity(model.groups.len());

    for (handle, group) in model.groups.iter() {
        if group.target == "host" {
            updates.push((handle, None));
            continue;
        }
        let h = model.targets.lookup(&group.target);
        if h.is_none() {
            diags.push(
                InternDiagnostic::error(format!(
                    "group '{}' references unknown target '{}'",
                    group.name, group.target
                ))
                .with_optional_span(group.span.clone()),
            );
        }
        updates.push((handle, h));
    }

    for (handle, target) in updates {
        if let Some(group) = model.groups.get_mut(handle) {
            group.target_handle = target;
        }
    }
}

/// Per-crate resolution result.
///
/// `deps` is an ordered list of `(dep_key, resolved_handle)` matching the
/// iteration order of `BTreeMap::iter` on the original `crate.deps`. Only
/// project-crate resolutions land as `Some`; vendored external deps resolve
/// to `None` with no diagnostic (that's not an error).
type CrateResolution = (
    Handle<CrateDef>,
    Option<Handle<GroupDef>>,
    Option<Option<Handle<TargetDef>>>,
    Vec<(String, Option<Handle<CrateDef>>)>,
    Vec<(String, Option<Handle<CrateDef>>)>,
);

fn intern_crates(model: &mut BuildModel, diags: &mut Vec<InternDiagnostic>) {
    let mut updates: Vec<CrateResolution> = Vec::with_capacity(model.crates.len());

    for (handle, krate) in model.crates.iter() {
        // Group ref is mandatory.
        let group_h = model.groups.lookup(&krate.group);
        if group_h.is_none() {
            diags.push(
                InternDiagnostic::error(format!(
                    "crate '{}' references unknown group '{}'",
                    krate.name, krate.group
                ))
                .with_optional_span(krate.span.clone()),
            );
        }

        // Target ref is optional and uses the "host" sentinel convention.
        let target_h = if krate.target.is_empty() || krate.target == "host" {
            None
        } else {
            let h = model.targets.lookup(&krate.target);
            if h.is_none() {
                diags.push(
                    InternDiagnostic::error(format!(
                        "crate '{}' references unknown target '{}'",
                        krate.name, krate.target
                    ))
                    .with_optional_span(krate.span.clone()),
                );
            }
            Some(h)
        };

        let deps = resolve_deps(model, krate, &krate.deps, diags, "deps");
        let dev_deps = resolve_deps(model, krate, &krate.dev_deps, diags, "dev-deps");

        updates.push((handle, group_h, target_h, deps, dev_deps));
    }

    for (handle, group_h, target_h, deps, dev_deps) in updates {
        if let Some(krate) = model.crates.get_mut(handle) {
            krate.group_handle = group_h;
            if let Some(v) = target_h {
                krate.target_handle = v;
            }
            for (key, resolved) in deps {
                if let Some(dep) = krate.deps.get_mut(&key) {
                    dep.crate_handle = resolved;
                }
            }
            for (key, resolved) in dev_deps {
                if let Some(dep) = krate.dev_deps.get_mut(&key) {
                    dep.crate_handle = resolved;
                }
            }
        }
    }
}

fn resolve_deps(
    model: &BuildModel,
    krate: &CrateDef,
    deps: &std::collections::BTreeMap<String, crate::build_model::DepDef>,
    diags: &mut Vec<InternDiagnostic>,
    kind: &str,
) -> Vec<(String, Option<Handle<CrateDef>>)> {
    let mut out = Vec::with_capacity(deps.len());
    for (extern_name, dep) in deps.iter() {
        // 1. Project-crate lookup.
        if let Some(h) = model.crates.lookup(&dep.crate_name) {
            out.push((extern_name.clone(), Some(h)));
            continue;
        }
        // 2. External dep fallback — no handle populated, and no diagnostic.
        if model.external_deps.lookup(&dep.crate_name).is_some() {
            out.push((extern_name.clone(), None));
            continue;
        }
        // 3. Dangling.
        diags.push(
            InternDiagnostic::error(format!(
                "dep '{}' in crate '{}' ({}) references unknown crate '{}'",
                extern_name, krate.name, kind, dep.crate_name
            ))
            .with_optional_span(dep.span.clone().or_else(|| krate.span.clone())),
        );
        out.push((extern_name.clone(), None));
    }
    out
}

/// Outer per-pipeline, inner per-stage, innermost per-input resolved handle.
type PipelineInputResolution = (Handle<PipelineDef>, Vec<Vec<Option<Handle<GroupDef>>>>);

fn intern_pipelines(model: &mut BuildModel, diags: &mut Vec<InternDiagnostic>) {
    // For each pipeline, build a parallel Vec<Vec<Option<Handle>>>: outer
    // per stage, inner per input.
    let mut updates: Vec<PipelineInputResolution> = Vec::with_capacity(model.pipelines.len());

    for (handle, pipeline) in model.pipelines.iter() {
        let mut per_stage = Vec::with_capacity(pipeline.stages.len());
        for stage in &pipeline.stages {
            let mut resolved = Vec::with_capacity(stage.inputs.len());
            for input in &stage.inputs {
                let h = model.groups.lookup(input);
                if h.is_none() {
                    diags.push(InternDiagnostic::error(format!(
                        "pipeline '{}' stage '{}' references unknown group '{}'",
                        pipeline.name, stage.name, input
                    )));
                }
                resolved.push(h);
            }
            per_stage.push(resolved);
        }
        updates.push((handle, per_stage));
    }

    for (handle, per_stage) in updates {
        if let Some(pipeline) = model.pipelines.get_mut(handle) {
            for (stage, resolved) in pipeline.stages.iter_mut().zip(per_stage) {
                stage.inputs_handles = resolved;
            }
        }
    }
}

/// Resolve `EspEntry::source_crate` names to crate handles. Dangling
/// references push a diagnostic and leave the handle `None` — the DAG
/// builder will then skip that entry, so a missing crate name is
/// non-fatal (diagnostic-only) at this layer.
fn intern_esps(model: &mut BuildModel, diags: &mut Vec<InternDiagnostic>) {
    type EspResolution = (Handle<EspDef>, Vec<Option<Handle<CrateDef>>>);
    let mut updates: Vec<EspResolution> = Vec::with_capacity(model.esps.len());

    for (handle, esp) in model.esps.iter() {
        let mut resolved = Vec::with_capacity(esp.entries.len());
        for entry in &esp.entries {
            let h = model.crates.lookup(&entry.source_crate);
            if h.is_none() {
                diags.push(
                    InternDiagnostic::error(format!(
                        "esp(\"{}\").add: source crate '{}' does not exist in the model",
                        esp.name, entry.source_crate
                    ))
                    .with_optional_span(esp.span.clone()),
                );
            }
            resolved.push(h);
        }
        updates.push((handle, resolved));
    }

    for (handle, resolved) in updates {
        if let Some(esp) = model.esps.get_mut(handle) {
            for (entry, handle) in esp.entries.iter_mut().zip(resolved) {
                entry.source_crate_handle = handle;
            }
        }
    }
}

/// Resolve [`BuildStepDef::runner`] (string crate name) and every
/// [`BuildStepInput::Crate { crate_name, .. }`] into their corresponding
/// [`Handle<CrateDef>`]. Dangling references push a diagnostic; the
/// handle is left `None` and the validate pass surfaces structural
/// issues (runner must be host bin, etc.) on the resolved handles.
fn intern_build_steps(model: &mut BuildModel, diags: &mut Vec<InternDiagnostic>) {
    /// One per build step: (handle, resolved_runner, resolved_inputs in
    /// BTreeMap iteration order). The inputs vec parallels
    /// `BuildStepDef::inputs.iter()` so we can apply the resolution
    /// without re-keying.
    type BuildStepResolution = (
        Handle<BuildStepDef>,
        Option<Handle<CrateDef>>,
        Vec<Option<Handle<CrateDef>>>,
    );
    let mut updates: Vec<BuildStepResolution> = Vec::with_capacity(model.build_steps.len());

    for (handle, step) in model.build_steps.iter() {
        // Runner: empty string was already diagnosed at builder time.
        // Skip the lookup for empty so we don't push a duplicate
        // diagnostic that says "unknown runner ''".
        let runner_handle = if step.runner.is_empty() {
            None
        } else {
            let h = model.crates.lookup(&step.runner);
            if h.is_none() {
                diags.push(
                    InternDiagnostic::error(format!(
                        "build_step(\"{}\").runner references unknown crate '{}'",
                        step.name, step.runner
                    ))
                    .with_optional_span(step.span.clone()),
                );
            }
            h
        };

        let mut input_handles = Vec::with_capacity(step.inputs.len());
        for (env_key, input) in &step.inputs {
            let resolved = match input {
                BuildStepInput::Crate { crate_name, .. } => {
                    let h = model.crates.lookup(crate_name);
                    if h.is_none() {
                        diags.push(
                            InternDiagnostic::error(format!(
                                "build_step(\"{}\").input(\"{}\") references unknown \
                                 crate '{}'",
                                step.name, env_key, crate_name
                            ))
                            .with_optional_span(step.span.clone()),
                        );
                    }
                    h
                }
                // File and Literal inputs don't reference any sibling.
                BuildStepInput::File { .. } | BuildStepInput::Literal { .. } => None,
            };
            input_handles.push(resolved);
        }
        updates.push((handle, runner_handle, input_handles));
    }

    for (handle, runner_handle, input_handles) in updates {
        if let Some(step) = model.build_steps.get_mut(handle) {
            step.runner_handle = runner_handle;
            // Apply input handles in BTreeMap iteration order, matching
            // the collection order above.
            let mut iter = input_handles.into_iter();
            for input in step.inputs.values_mut() {
                if let BuildStepInput::Crate { crate_handle, .. } = input {
                    *crate_handle = iter.next().flatten();
                } else {
                    // Advance past File/Literal placeholder.
                    let _ = iter.next();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProfileDef, ProjectDef, TargetDef};

    fn fixture_with_target_and_profile() -> RawBuildModel {
        let mut raw = RawBuildModel::default();
        raw.model.project = Some(ProjectDef {
            name: "demo".into(),
            version: "0.1.0".into(),
            ..Default::default()
        });
        raw.model.targets.insert(
            "x86_64-unknown-none".into(),
            TargetDef {
                name: "x86_64-unknown-none".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        raw.model.profiles.insert(
            "dev".into(),
            ProfileDef {
                name: "dev".into(),
                target: Some("x86_64-unknown-none".into()),
                ..Default::default()
            },
        );
        raw
    }

    #[test]
    fn intern_resolves_profile_target_handle() {
        let raw = fixture_with_target_and_profile();
        let (model, diags) = intern(raw);
        assert!(diags.is_empty(), "no dangling refs: {diags:?}");
        let ph = model.profiles.lookup("dev").expect("dev profile");
        let p = model.profiles.get(ph).unwrap();
        assert!(
            p.target_handle.is_some(),
            "intern must populate profile.target_handle"
        );
    }

    #[test]
    fn intern_diagnoses_dangling_target() {
        let mut raw = RawBuildModel::default();
        raw.model.project = Some(ProjectDef {
            name: "demo".into(),
            version: "0.1.0".into(),
            ..Default::default()
        });
        raw.model.profiles.insert(
            "dev".into(),
            ProfileDef {
                name: "dev".into(),
                target: Some("nonexistent".into()),
                ..Default::default()
            },
        );
        let (_, diags) = intern(raw);
        assert!(
            diags.iter().any(|d| d.message.contains("unknown target 'nonexistent'")),
            "expected dangling-target diagnostic, got: {diags:?}"
        );
    }
}
