//! Raw* mirror types for `RawBuildModel`.
//!
//! Each type here mirrors a corresponding `Def` in
//! [`crate::build_model`] but stores cross-references as name strings
//! instead of `Handle<...>`. `gluon-config` builders write these; the
//! [`crate::intern`] pass converts them to handles in the final
//! [`crate::build_model::BuildModel`].

use crate::build_model::{default_true, BootMode};
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
}
