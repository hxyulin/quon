//! Boundary between `RawBuildModel` (gluon-config emits) and
//! `BuildModel` (gluon-core consumes).
//!
//! After Phase C of Stage 4, intern is a total transform: it consumes
//! a [`RawBuildModel`] (cross-refs as name strings) and produces a
//! [`BuildModel`] (cross-refs as handles, name strings still present
//! for the parallel-pair phase). Phase D removes the redundant name
//! fields from BuildModel; Phase F moves config_options to an arena;
//! Phase G makes qemu/bootloader Option.

use crate::build_model::{
    BootloaderDef, BuildModel, BuildStepDef, BuildStepInput, CrateDef, DepDef, EspDef,
    EspEntry, GroupDef, PipelineDef, PipelineStep, ProfileDef, TargetSpec,
};
use crate::handle::{Arena, Handle};
use crate::raw::RawBuildModel;
use crate::raw_defs::{
    RawBootloaderDef, RawBuildStepDef, RawBuildStepInput, RawCrateDef, RawDepDef, RawEspDef,
    RawGroupDef, RawPipelineDef, RawProfileDef,
};
use crate::source::SourceSpan;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct InternDiagnostic {
    pub message: String,
    pub span: Option<SourceSpan>,
}

impl InternDiagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self { message: message.into(), span: None }
    }
    pub fn with_span(mut self, span: SourceSpan) -> Self { self.span = Some(span); self }
    pub fn with_optional_span(mut self, span: Option<SourceSpan>) -> Self {
        if let Some(s) = span { self.span = Some(s); }
        self
    }
}

pub fn intern(raw: RawBuildModel) -> (BuildModel, Vec<InternDiagnostic>) {
    let mut diags = Vec::new();
    // Pass 1 — copy types that need no resolution.
    let mut model = BuildModel {
        project: raw.project,
        targets: raw.targets,
        rules: raw.rules,
        external_deps: raw.external_deps,
        images: raw.images,
        config_options: raw.config_options,
        presets: raw.presets,
        qemu: raw.qemu,
        ..Default::default()
    };

    // Pass 2 — resolve arenas in dependency order:
    //   targets → groups → crates → profiles → bootloader → esps → build_steps → pipelines.
    intern_groups(&raw.groups, &model.targets, &mut model.groups, &mut diags);
    intern_crates(
        &raw.crates,
        &model.targets,
        &model.groups,
        &model.external_deps,
        &mut model.crates,
        &mut diags,
    );
    intern_profiles(&raw.profiles, &model.targets, &model.crates, &mut model.profiles, &mut diags);
    model.bootloader = intern_bootloader(&raw.bootloader, &model.crates, &mut diags);
    intern_esps(&raw.esps, &model.crates, &mut model.esps, &mut diags);
    intern_build_steps(&raw.build_steps, &model.crates, &mut model.build_steps, &mut diags);
    intern_pipelines(&raw.pipelines, &model.groups, &mut model.pipelines, &mut diags);

    (model, diags)
}

// One-helper-per-arena. Each helper iterates the raw arena, builds
// the post-intern Def by looking up names in the dependency arenas,
// and inserts into the destination arena. Names not found push a
// diagnostic and leave the corresponding handle field `None` in
// BuildModel (which is what `validate.rs` consumed in the old
// engine path).

fn intern_groups(
    raw: &Arena<RawGroupDef>,
    targets: &Arena<crate::build_model::TargetDef>,
    out: &mut Arena<GroupDef>,
    diags: &mut Vec<InternDiagnostic>,
) {
    for (_, g) in raw.iter() {
        let target = if g.target == "host" {
            TargetSpec::Host
        } else {
            match targets.lookup(&g.target) {
                Some(h) => TargetSpec::Cross(h),
                None => {
                    diags.push(
                        InternDiagnostic::error(format!(
                            "group '{}' references unknown target '{}'",
                            g.name, g.target
                        ))
                        .with_optional_span(g.span.clone()),
                    );
                    // Fall back so the Def is well-formed; the diagnostic
                    // is the signal that something was wrong.
                    TargetSpec::Host
                }
            }
        };
        let _ = out.insert(
            g.name.clone(),
            GroupDef {
                name: g.name.clone(),
                target,
                default_edition: g.default_edition.clone(),
                crates: g.crates.clone(),
                shared_flags: g.shared_flags.clone(),
                is_project: g.is_project,
                config: g.config,
                no_sysroot: g.no_sysroot,
                span: g.span.clone(),
            },
        );
    }
}

fn intern_crates(
    raw: &Arena<RawCrateDef>,
    targets: &Arena<crate::build_model::TargetDef>,
    groups: &Arena<GroupDef>,
    external_deps: &Arena<crate::build_model::ExternalDepDef>,
    out: &mut Arena<CrateDef>,
    diags: &mut Vec<InternDiagnostic>,
) {
    // Two passes: insert all crates first (so deps can resolve to
    // siblings), then re-walk to populate `deps`/`dev_deps` handles.
    //
    // CrateDef.group is mandatory, so a crate with an unknown group is
    // skipped entirely (with a diagnostic). Pass 2 silently skips crates
    // that didn't make it into `out`.
    for (_, c) in raw.iter() {
        let Some(group) = groups.lookup(&c.group) else {
            diags.push(InternDiagnostic::error(format!(
                "crate '{}' references unknown group '{}'",
                c.name, c.group
            )).with_optional_span(c.span.clone()));
            continue;
        };
        let target = if c.target.is_empty() || c.target == "host" {
            TargetSpec::Host
        } else {
            match targets.lookup(&c.target) {
                Some(h) => TargetSpec::Cross(h),
                None => {
                    diags.push(InternDiagnostic::error(format!(
                        "crate '{}' references unknown target '{}'",
                        c.name, c.target
                    )).with_optional_span(c.span.clone()));
                    TargetSpec::Host
                }
            }
        };
        let _ = out.insert(
            c.name.clone(),
            CrateDef {
                name: c.name.clone(),
                path: c.path.clone(),
                edition: c.edition.clone(),
                crate_type: c.crate_type,
                target,
                deps: BTreeMap::new(),       // filled in pass 2
                dev_deps: BTreeMap::new(),
                features: c.features.clone(),
                root: c.root.clone(),
                linker_script: c.linker_script.clone(),
                group,
                is_project_crate: c.is_project_crate,
                cfg_flags: c.cfg_flags.clone(),
                rustc_flags: c.rustc_flags.clone(),
                requires_config: c.requires_config.clone(),
                artifact_deps: c.artifact_deps.clone(),
                artifact_env: c.artifact_env.clone(),
                span: c.span.clone(),
            },
        );
    }
    // Pass 2: resolve deps now that all crates exist in `out`.
    let snapshot: Vec<(String, RawCrateDef)> = raw.iter().map(|(_, c)| (c.name.clone(), c.clone())).collect();
    for (name, c) in snapshot {
        let Some(h) = out.lookup(&name) else { continue; };
        let deps = resolve_dep_map(&c.deps, &c.name, "deps", out, external_deps, diags, c.span.clone());
        let dev_deps = resolve_dep_map(&c.dev_deps, &c.name, "dev-deps", out, external_deps, diags, c.span.clone());
        if let Some(slot) = out.get_mut(h) {
            slot.deps = deps;
            slot.dev_deps = dev_deps;
        }
    }
}

fn resolve_dep_map(
    raw: &BTreeMap<String, RawDepDef>,
    crate_name: &str,
    kind: &str,
    crates: &Arena<CrateDef>,
    external_deps: &Arena<crate::build_model::ExternalDepDef>,
    diags: &mut Vec<InternDiagnostic>,
    span: Option<SourceSpan>,
) -> BTreeMap<String, DepDef> {
    let mut out = BTreeMap::new();
    for (extern_name, d) in raw.iter() {
        let crate_handle = crates.lookup(&d.crate_name);
        if crate_handle.is_none() && external_deps.lookup(&d.crate_name).is_none() {
            diags.push(InternDiagnostic::error(format!(
                "dep '{}' in crate '{}' ({}) references unknown crate '{}'",
                extern_name, crate_name, kind, d.crate_name
            )).with_optional_span(d.span.clone().or_else(|| span.clone())));
        }
        out.insert(
            extern_name.clone(),
            DepDef {
                crate_name: d.crate_name.clone(),
                crate_handle,
                features: d.features.clone(),
                version: d.version.clone(),
                optional: d.optional,
                default_features: d.default_features,
                span: d.span.clone(),
            },
        );
    }
    out
}

fn intern_profiles(
    raw: &Arena<RawProfileDef>,
    targets: &Arena<crate::build_model::TargetDef>,
    crates: &Arena<CrateDef>,
    out: &mut Arena<ProfileDef>,
    diags: &mut Vec<InternDiagnostic>,
) {
    // Two passes: insert all profiles, then resolve `inherits` handles.
    for (_, p) in raw.iter() {
        let target_handle = p.target.as_deref().and_then(|name| {
            let h = targets.lookup(name);
            if h.is_none() {
                diags.push(InternDiagnostic::error(format!(
                    "profile '{}' references unknown target '{}'",
                    p.name, name
                )).with_optional_span(p.span.clone()));
            }
            h
        });
        let boot_binary_handle = p.boot_binary.as_deref().and_then(|name| {
            let h = crates.lookup(name);
            if h.is_none() {
                diags.push(InternDiagnostic::error(format!(
                    "profile '{}' references unknown boot binary crate '{}'",
                    p.name, name
                )).with_optional_span(p.span.clone()));
            }
            h
        });
        let _ = out.insert(
            p.name.clone(),
            ProfileDef {
                name: p.name.clone(),
                inherits: None,                  // populated in pass 2
                target: target_handle,
                opt_level: p.opt_level,
                debug_info: p.debug_info,
                lto: p.lto.clone(),
                boot_binary: boot_binary_handle,
                preset: p.preset.clone(),
                config: p.config.clone(),
                qemu_memory: p.qemu_memory,
                qemu_cores: p.qemu_cores,
                qemu_extra_args: p.qemu_extra_args.clone(),
                qemu_boot_mode: p.qemu_boot_mode,
                test_timeout: p.test_timeout,
                span: p.span.clone(),
            },
        );
    }
    // Pass 2: resolve `inherits`.
    let snapshot: Vec<(String, Option<String>, Option<SourceSpan>)> =
        raw.iter().map(|(_, p)| (p.name.clone(), p.inherits.clone(), p.span.clone())).collect();
    for (name, inherits, span) in snapshot {
        let h = out.lookup(&name).expect("inserted in pass 1");
        if let Some(parent_name) = inherits {
            let parent_h = out.lookup(&parent_name);
            if parent_h.is_none() {
                diags.push(InternDiagnostic::error(format!(
                    "profile '{}' inherits from unknown profile '{}'",
                    name, parent_name
                )).with_optional_span(span));
            }
            if let Some(slot) = out.get_mut(h) {
                slot.inherits = parent_h;
            }
        }
    }
}

fn intern_bootloader(
    raw: &RawBootloaderDef,
    crates: &Arena<CrateDef>,
    diags: &mut Vec<InternDiagnostic>,
) -> BootloaderDef {
    let entry_crate_handle = raw.entry_crate.as_deref().and_then(|name| {
        let h = crates.lookup(name);
        if h.is_none() {
            diags.push(InternDiagnostic::error(format!(
                "bootloader.entry_crate references unknown crate '{}'", name
            )).with_optional_span(raw.span.clone()));
        }
        h
    });
    BootloaderDef {
        kind: raw.kind.clone(),
        protocol: raw.protocol.clone(),
        entry_crate: raw.entry_crate.clone(),
        entry_crate_handle,
        config_file: raw.config_file.clone(),
        extras: raw.extras.clone(),
        span: raw.span.clone(),
    }
}

fn intern_esps(
    raw: &Arena<RawEspDef>,
    crates: &Arena<CrateDef>,
    out: &mut Arena<EspDef>,
    diags: &mut Vec<InternDiagnostic>,
) {
    for (_, e) in raw.iter() {
        let entries = e.entries.iter().map(|en| {
            let h = crates.lookup(&en.source_crate);
            if h.is_none() {
                diags.push(InternDiagnostic::error(format!(
                    "esp(\"{}\").add: source crate '{}' does not exist in the model",
                    e.name, en.source_crate
                )).with_optional_span(e.span.clone()));
            }
            EspEntry {
                source_crate: en.source_crate.clone(),
                source_crate_handle: h,
                dest_path: en.dest_path.clone(),
            }
        }).collect();
        let _ = out.insert(
            e.name.clone(),
            EspDef { name: e.name.clone(), entries, span: e.span.clone() },
        );
    }
}

fn intern_build_steps(
    raw: &Arena<RawBuildStepDef>,
    crates: &Arena<CrateDef>,
    out: &mut Arena<BuildStepDef>,
    diags: &mut Vec<InternDiagnostic>,
) {
    for (_, s) in raw.iter() {
        let runner_handle = if s.runner.is_empty() {
            None
        } else {
            let h = crates.lookup(&s.runner);
            if h.is_none() {
                diags.push(InternDiagnostic::error(format!(
                    "build_step(\"{}\").runner references unknown crate '{}'",
                    s.name, s.runner
                )).with_optional_span(s.span.clone()));
            }
            h
        };
        let inputs = s.inputs.iter().map(|(env_key, inp)| {
            let resolved = match inp {
                RawBuildStepInput::Crate { crate_name } => {
                    let h = crates.lookup(crate_name);
                    if h.is_none() {
                        diags.push(InternDiagnostic::error(format!(
                            "build_step(\"{}\").input(\"{}\") references unknown crate '{}'",
                            s.name, env_key, crate_name
                        )).with_optional_span(s.span.clone()));
                    }
                    BuildStepInput::Crate { crate_name: crate_name.clone(), crate_handle: h }
                }
                RawBuildStepInput::File { path } =>
                    BuildStepInput::File { path: path.clone() },
                RawBuildStepInput::Literal { value } =>
                    BuildStepInput::Literal { value: value.clone() },
            };
            (env_key.clone(), resolved)
        }).collect();
        let _ = out.insert(
            s.name.clone(),
            BuildStepDef {
                name: s.name.clone(),
                runner: s.runner.clone(),
                runner_handle,
                inputs,
                env: s.env.clone(),
                output: s.output.clone(),
                span: s.span.clone(),
            },
        );
    }
}

fn intern_pipelines(
    raw: &Arena<RawPipelineDef>,
    groups: &Arena<GroupDef>,
    out: &mut Arena<PipelineDef>,
    diags: &mut Vec<InternDiagnostic>,
) {
    for (_, p) in raw.iter() {
        let stages = p.stages.iter().map(|s| {
            let resolved: Vec<Option<Handle<GroupDef>>> = s.inputs.iter().map(|input| {
                let h = groups.lookup(input);
                if h.is_none() {
                    diags.push(InternDiagnostic::error(format!(
                        "pipeline '{}' stage '{}' references unknown group '{}'",
                        p.name, s.name, input
                    )));
                }
                h
            }).collect();
            PipelineStep {
                name: s.name.clone(),
                inputs: s.inputs.clone(),
                inputs_handles: resolved,
                rule: s.rule.clone(),
            }
        }).collect();
        let _ = out.insert(
            p.name.clone(),
            PipelineDef { name: p.name.clone(), stages, span: p.span.clone() },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_model::{ProjectDef, TargetDef};
    use crate::raw_defs::RawProfileDef;

    fn fixture_with_target_and_profile() -> RawBuildModel {
        let mut raw = RawBuildModel {
            project: Some(ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        raw.targets.insert(
            "x86_64-unknown-none".into(),
            TargetDef {
                name: "x86_64-unknown-none".into(),
                spec: "x86_64-unknown-none".into(),
                builtin: true,
                panic_strategy: None,
                span: None,
            },
        );
        raw.profiles.insert(
            "dev".into(),
            RawProfileDef {
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
        assert!(p.target.is_some());
    }

    #[test]
    fn intern_diagnoses_dangling_target() {
        let mut raw = RawBuildModel {
            project: Some(ProjectDef {
                name: "demo".into(),
                version: "0.1.0".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        raw.profiles.insert(
            "dev".into(),
            RawProfileDef {
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
