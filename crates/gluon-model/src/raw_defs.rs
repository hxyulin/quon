//! Raw* mirror types for `RawBuildModel`.
//!
//! Each type here mirrors a corresponding `Def` in
//! [`crate::build_model`] but stores cross-references as name strings
//! instead of `Handle<...>`. `gluon-config` builders write these; the
//! [`crate::intern`] pass converts them to handles in the final
//! [`crate::build_model::BuildModel`].

use crate::source::SourceSpan;
use serde::{Deserialize, Serialize};

fn default_true() -> bool { true }

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
}
