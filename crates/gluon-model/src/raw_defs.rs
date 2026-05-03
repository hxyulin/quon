//! Raw* mirror types for `RawBuildModel`.
//!
//! Each type here mirrors a corresponding `Def` in
//! [`crate::build_model`] but stores cross-references as name strings
//! instead of `Handle<...>`. `gluon-config` builders write these; the
//! [`crate::intern`] pass converts them to handles in the final
//! [`crate::build_model::BuildModel`].

use crate::build_model::{default_true, BootMode, CrateType};
use crate::handle::Named;
use crate::kconfig::ConfigValue;
use crate::source::SourceSpan;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Raw form of [`crate::build_model::DepDef`]: name strings, no
/// resolution. Intern resolves `crate_name` against project crates
/// first, then external deps; mismatch produces an [`crate::intern::InternDiagnostic`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawDepDef {
    pub crate_name: String,
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default = "default_true")]
    pub default_features: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Default for RawDepDef {
    fn default() -> Self {
        Self {
            crate_name: String::new(),
            features: Vec::new(),
            version: None,
            optional: false,
            default_features: true,
            span: None,
        }
    }
}

/// Raw form of [`crate::build_model::ProfileDef`]: cross-references
/// stored as name strings. Intern resolves `inherits`, `target`, and
/// `boot_binary` against the relevant arenas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawProfileDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_binary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_level: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug_info: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lto: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(default)]
    pub config: BTreeMap<String, ConfigValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_memory: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_extra_args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qemu_boot_mode: Option<BootMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_timeout: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Named for RawProfileDef {
    fn name(&self) -> &str {
        &self.name
    }
}

/// Raw form of [`crate::build_model::GroupDef`]: target carried as
/// the name string (or the `"host"` sentinel). Intern resolves to
/// [`crate::build_model::TargetSpec`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawGroupDef {
    pub name: String,
    pub target: String,
    pub default_edition: String,
    pub crates: Vec<String>,
    pub shared_flags: Vec<String>,
    pub is_project: bool,
    pub config: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_sysroot: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Default for RawGroupDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            target: "host".into(),
            default_edition: "2024".into(),
            crates: Vec::new(),
            shared_flags: Vec::new(),
            is_project: true,
            config: false,
            no_sysroot: false,
            span: None,
        }
    }
}

impl Named for RawGroupDef {
    fn name(&self) -> &str { &self.name }
}

/// Raw form of [`crate::build_model::CrateDef`]: `target` and `group`
/// carried as name strings; deps are [`RawDepDef`]s.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawCrateDef {
    pub name: String,
    pub path: String,
    pub edition: String,
    pub crate_type: CrateType,
    pub target: String,
    pub deps: BTreeMap<String, RawDepDef>,
    pub dev_deps: BTreeMap<String, RawDepDef>,
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linker_script: Option<String>,
    pub group: String,
    pub is_project_crate: bool,
    pub cfg_flags: Vec<String>,
    pub rustc_flags: Vec<String>,
    pub requires_config: Vec<String>,
    #[serde(default)]
    pub artifact_deps: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifact_env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Named for RawCrateDef {
    fn name(&self) -> &str { &self.name }
}

/// Raw form of [`crate::build_model::BootloaderDef`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawBootloaderDef {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_crate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extras: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Raw form of [`crate::build_model::EspDef`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawEspDef {
    pub name: String,
    #[serde(default)]
    pub entries: Vec<RawEspEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Named for RawEspDef {
    fn name(&self) -> &str { &self.name }
}

/// Raw form of [`crate::build_model::EspEntry`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEspEntry {
    pub source_crate: String,
    pub dest_path: String,
}

/// Raw form of [`crate::build_model::BuildStepDef`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawBuildStepDef {
    pub name: String,
    pub runner: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, RawBuildStepInput>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl Named for RawBuildStepDef {
    fn name(&self) -> &str { &self.name }
}

/// Raw form of [`crate::build_model::BuildStepInput`]. The `Crate`
/// variant carries a name string instead of a handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawBuildStepInput {
    Crate { crate_name: String },
    File { path: String },
    Literal { value: String },
}

/// Raw form of [`crate::build_model::PipelineStep`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawPipelineStep {
    pub name: String,
    pub inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_dep_def_round_trips_through_json() {
        let d = RawDepDef {
            crate_name: "log".into(),
            features: vec!["std".into()],
            version: Some("0.4".into()),
            optional: true,
            default_features: false,
            span: None,
        };
        let json = serde_json::to_string(&d).unwrap();
        let de: RawDepDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.crate_name, "log");
        assert_eq!(de.features, vec!["std".to_string()]);
        assert_eq!(de.version.as_deref(), Some("0.4"));
        assert!(de.optional);
        assert!(!de.default_features);
    }

    #[test]
    fn raw_profile_def_round_trips_through_json() {
        let p = RawProfileDef {
            name: "dev".into(),
            inherits: Some("base".into()),
            target: Some("x86_64".into()),
            boot_binary: Some("kernel".into()),
            opt_level: Some(0),
            debug_info: Some(true),
            lto: Some("off".into()),
            preset: Some("baseline".into()),
            config: BTreeMap::new(),
            qemu_memory: Some(512),
            qemu_cores: Some(2),
            qemu_extra_args: None,
            qemu_boot_mode: None,
            test_timeout: Some(30),
            span: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let de: RawProfileDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.name, "dev");
        assert_eq!(de.inherits.as_deref(), Some("base"));
        assert_eq!(de.target.as_deref(), Some("x86_64"));
        assert_eq!(de.boot_binary.as_deref(), Some("kernel"));
        assert_eq!(de.opt_level, Some(0));
        assert_eq!(de.qemu_memory, Some(512));
    }

    #[test]
    fn raw_group_def_round_trips_through_json() {
        let g = RawGroupDef {
            name: "kernel".into(),
            target: "x86_64".into(),
            default_edition: "2024".into(),
            crates: vec!["main".into()],
            shared_flags: vec!["-O".into()],
            is_project: true,
            config: false,
            no_sysroot: false,
            span: None,
        };
        let json = serde_json::to_string(&g).unwrap();
        let de: RawGroupDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.target, "x86_64");
        assert_eq!(de.crates, vec!["main".to_string()]);
    }

    #[test]
    fn raw_bootloader_def_round_trips() {
        let b = RawBootloaderDef {
            kind: "uefi".into(),
            protocol: Some("gop".into()),
            entry_crate: Some("boot".into()),
            config_file: Some("limine.cfg".into()),
            extras: BTreeMap::new(),
            span: None,
        };
        let json = serde_json::to_string(&b).unwrap();
        let de: RawBootloaderDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.kind, "uefi");
        assert_eq!(de.entry_crate.as_deref(), Some("boot"));
    }

    #[test]
    fn raw_esp_round_trips() {
        let e = RawEspDef {
            name: "default".into(),
            entries: vec![RawEspEntry {
                source_crate: "boot".into(),
                dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
            }],
            span: None,
        };
        let json = serde_json::to_string(&e).unwrap();
        let de: RawEspDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.entries.len(), 1);
        assert_eq!(de.entries[0].source_crate, "boot");
    }

    #[test]
    fn raw_build_step_round_trips() {
        let mut s = RawBuildStepDef {
            name: "stripper".into(),
            runner: "elf-stripper".into(),
            inputs: BTreeMap::new(),
            env: BTreeMap::new(),
            output: "stripped.elf".into(),
            span: None,
        };
        s.inputs.insert(
            "ELF".into(),
            RawBuildStepInput::Crate { crate_name: "kernel".into() },
        );
        s.inputs.insert(
            "FOO".into(),
            RawBuildStepInput::File { path: "tools/foo".into() },
        );
        s.inputs.insert(
            "BAR".into(),
            RawBuildStepInput::Literal { value: "yes".into() },
        );
        let json = serde_json::to_string(&s).unwrap();
        let de: RawBuildStepDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.runner, "elf-stripper");
        assert_eq!(de.inputs.len(), 3);
        match &de.inputs["ELF"] {
            RawBuildStepInput::Crate { crate_name } => assert_eq!(crate_name, "kernel"),
            other => panic!("expected Crate, got {other:?}"),
        }
    }

    #[test]
    fn raw_pipeline_step_round_trips() {
        let s = RawPipelineStep {
            name: "compile".into(),
            inputs: vec!["kernel-group".into(), "shared".into()],
            rule: Some("postprocess".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        let de: RawPipelineStep = serde_json::from_str(&json).unwrap();
        assert_eq!(de.inputs.len(), 2);
        assert_eq!(de.rule.as_deref(), Some("postprocess"));
    }

    #[test]
    fn raw_crate_def_round_trips_through_json() {
        let mut c = RawCrateDef {
            name: "main".into(),
            path: "src/main".into(),
            edition: "2024".into(),
            crate_type: crate::build_model::CrateType::Bin,
            target: "x86_64".into(),
            deps: BTreeMap::new(),
            dev_deps: BTreeMap::new(),
            features: Vec::new(),
            root: None,
            linker_script: None,
            group: "kernel".into(),
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: BTreeMap::new(),
            span: None,
        };
        c.deps.insert(
            "log".into(),
            RawDepDef { crate_name: "log".into(), ..Default::default() },
        );
        let json = serde_json::to_string(&c).unwrap();
        let de: RawCrateDef = serde_json::from_str(&json).unwrap();
        assert_eq!(de.target, "x86_64");
        assert_eq!(de.group, "kernel");
        assert_eq!(de.deps.len(), 1);
        assert_eq!(de.deps["log"].crate_name, "log");
    }
}
