use crate::build_model::BuildModel;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Pre-intern build model: the shape `gluon-config` emits.
///
/// Stage 1 wraps a `BuildModel` whose `*_handle` fields are uniformly
/// `None`. Stage 4 will split this into a structurally distinct shape
/// (no parallel string fields) per spec D4.1; the public API stays the
/// same — `intern()` produces a `BuildModel` either way.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RawBuildModel {
    #[serde(flatten)]
    pub model: BuildModel,
    /// Paths recorded by `Config::include_kconfig`. The host-side
    /// `script_runner` parses these *before* intern and merges loaded
    /// options/presets into `model.config_options` / `model.presets`.
    /// Not part of the final `BuildModel`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kconfig_includes: Vec<PathBuf>,
    /// Stage-1 sidecar. `qemu(...)` and `bootloader(...)` are singletons;
    /// today's `BuildModel.qemu` / `.bootloader` are non-`Option` so we
    /// track "user actually called this" out-of-band. Stage 4 (D4.3)
    /// makes the model fields `Option<...>` and removes these flags.
    #[serde(skip)]
    pub qemu_was_set: bool,
    #[serde(skip)]
    pub bootloader_was_set: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProjectDef;

    #[test]
    fn raw_build_model_round_trips_empty() {
        let raw = RawBuildModel::default();
        let json = serde_json::to_string(&raw).expect("serde");
        let de: RawBuildModel = serde_json::from_str(&json).expect("de");
        assert!(de.model.project.is_none());
        assert!(de.kconfig_includes.is_empty());
    }

    #[test]
    fn raw_build_model_round_trips_with_project_and_kconfig_path() {
        let mut raw = RawBuildModel::default();
        raw.model.project = Some(ProjectDef {
            name: "demo".into(),
            version: "0.1.0".into(),
            ..Default::default()
        });
        raw.kconfig_includes.push(PathBuf::from("./options.kconfig"));
        raw.qemu_was_set = true;
        raw.bootloader_was_set = true;

        let json = serde_json::to_string(&raw).expect("serde");
        let de: RawBuildModel = serde_json::from_str(&json).expect("de");
        assert_eq!(de.model.project.as_ref().unwrap().name, "demo");
        assert_eq!(de.kconfig_includes, vec![PathBuf::from("./options.kconfig")]);
        // Sanity-check the #[serde(skip)] contract: sidecar flags must NOT
        // round-trip — even if `raw` has them set true before serialisation,
        // the deserialised value sees them as false (the bool default).
        assert!(!de.qemu_was_set, "qemu_was_set must be skipped on deserialize");
        assert!(!de.bootloader_was_set, "bootloader_was_set must be skipped on deserialize");
    }
}
