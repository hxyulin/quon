use crate::build_model::{
    ExternalDepDef, ImageDef, ProjectDef, QemuDef, RuleDef,
    TargetDef,
};
use crate::handle::Arena;
use crate::kconfig::{ConfigOptionDef, PresetDef};
use crate::raw_defs::{
    RawBootloaderDef, RawBuildStepDef, RawCrateDef, RawEspDef, RawGroupDef,
    RawPipelineDef, RawProfileDef,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Pre-intern build model: the shape `gluon-config` emits and
/// `gluon-core::script_runner` deserialises from `gluon.rs` stdout.
///
/// Cross-references live as name strings inside the [`raw_defs`]
/// types. [`crate::intern::intern`] is a total transform that
/// resolves those strings against the relevant arenas, producing a
/// [`crate::build_model::BuildModel`] with handles populated.
///
/// Types that hold no cross-references are reused verbatim: targets,
/// rules, external deps, images, presets, project, qemu, config
/// options. Singleton `bootloader` and `qemu` are `Option<...>` —
/// `Some` means the user wrote the corresponding `cfg.<x>(...)` block,
/// `None` means they didn't. Duplicate detection at the builder layer
/// reads `field.is_some()` rather than a sidecar `*_was_set` flag
/// (Phase G of Stage 4 retired the sidecars).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RawBuildModel {
    pub project: Option<ProjectDef>,
    pub targets: Arena<TargetDef>,
    pub profiles: Arena<RawProfileDef>,
    pub groups: Arena<RawGroupDef>,
    pub crates: Arena<RawCrateDef>,
    pub rules: Arena<RuleDef>,
    pub pipelines: Arena<RawPipelineDef>,
    pub external_deps: Arena<ExternalDepDef>,
    #[serde(default)]
    pub esps: Arena<RawEspDef>,
    pub config_options: Arena<ConfigOptionDef>,
    pub presets: BTreeMap<String, PresetDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootloader: Option<RawBootloaderDef>,
    #[serde(default)]
    pub images: Arena<ImageDef>,
    #[serde(default)]
    pub build_steps: Arena<RawBuildStepDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu: Option<QemuDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kconfig_includes: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_build_model_round_trips_empty() {
        let raw = RawBuildModel::default();
        let json = serde_json::to_string(&raw).expect("serde");
        let de: RawBuildModel = serde_json::from_str(&json).expect("de");
        assert!(de.project.is_none());
        assert!(de.profiles.is_empty());
        assert!(de.kconfig_includes.is_empty());
    }

    #[test]
    fn raw_build_model_round_trips_with_data() {
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
                target: Some("x86_64".into()),
                ..Default::default()
            },
        );
        raw.kconfig_includes.push(PathBuf::from("./options.kconfig"));

        let json = serde_json::to_string(&raw).expect("serde");
        let de: RawBuildModel = serde_json::from_str(&json).expect("de");
        assert_eq!(de.project.as_ref().unwrap().name, "demo");
        let h = de.profiles.lookup("dev").expect("dev profile");
        assert_eq!(de.profiles.get(h).unwrap().target.as_deref(), Some("x86_64"));
        assert_eq!(de.kconfig_includes.len(), 1);
    }

    #[test]
    fn singleton_options_default_to_none() {
        // Phase G of Stage 4: bootloader and qemu are `Option<...>` and
        // their absence is communicated by `None`, not by sidecar
        // `*_was_set` flags.  `skip_serializing_if = "Option::is_none"`
        // keeps the wire format empty when nothing is configured.
        let raw = RawBuildModel::default();
        assert!(raw.bootloader.is_none());
        assert!(raw.qemu.is_none());
        let json = serde_json::to_string(&raw).expect("serde");
        assert!(
            !json.contains("\"qemu\""),
            "absent qemu must not be serialized, got: {json}"
        );
        assert!(
            !json.contains("\"bootloader\""),
            "absent bootloader must not be serialized, got: {json}"
        );
    }
}
