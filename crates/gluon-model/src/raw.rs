use crate::build_model::{
    DownloadDef, ExternalDepDef, ImageDef, ProjectDef, QemuDef, RuleDef, TargetDef,
};
use crate::handle::Arena;
use crate::kconfig::{ConfigOptionDef, PresetDef};
use crate::raw_defs::{
    RawBootTestDef, RawBootloaderDef, RawBuildStepDef, RawCargoWorkspace, RawCrateDef, RawEspDef,
    RawGroupDef, RawPipelineDef, RawProfileDef,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    #[serde(default)]
    pub downloads: Arena<DownloadDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kconfig_includes: Vec<PathBuf>,
    /// `cfg.cargo_workspace(dir)`. Consumed by manifest expansion
    /// before intern, like `kconfig_includes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_workspace: Option<RawCargoWorkspace>,
    /// `cfg.host_tests().package(name)`: workspace packages whose unit
    /// tests `gluon test` builds and runs on the host. Consumed by
    /// manifest expansion, like `cargo_workspace`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub host_tests: BTreeSet<String>,
    /// `cfg.boot_test(name)`: suites that boot a profile's run binary.
    #[serde(default)]
    pub boot_tests: Arena<RawBootTestDef>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // gluon.rs output reaches gluon-core as this JSON, so every field
        // skipped above must also carry `#[serde(default)]` to read back.
        let de: RawBuildModel = serde_json::from_str(&json).expect("de");
        assert!(de.project.is_none());
        assert!(de.bootloader.is_none());
        assert!(de.qemu.is_none());
        assert!(de.kconfig_includes.is_empty());
        assert!(de.cargo_workspace.is_none());
    }
}
