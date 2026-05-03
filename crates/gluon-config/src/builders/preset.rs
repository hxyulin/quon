//! Chainable builder for `PresetDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use gluon_model::{ConfigValue, PresetDef};

/// Chainable builder returned by [`Config::preset`].
pub struct PresetBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<()>,
}

impl<'cfg> PresetBuilder<'cfg> {
    /// Apply `f` to the `PresetDef` if this builder is not a duplicate.
    fn modify<F: FnOnce(&mut PresetDef)>(&mut self, f: F) {
        if !self.state.is_duplicate() {
            let name = self.state.name().to_string();
            if let Some(p) = self.cfg.raw.presets.get_mut(&name) {
                f(p);
            }
        }
    }

    /// Set the value of a config option in this preset's override map.
    pub fn set(mut self, name: &str, value: ConfigValue) -> Self {
        let n = name.to_string();
        self.modify(|p| {
            p.overrides.insert(n, value);
        });
        self
    }

    /// Declare that this preset inherits from a parent preset.
    pub fn inherits(mut self, parent: &str) -> Self {
        let p = parent.to_string();
        self.modify(|preset| preset.inherits = Some(p));
        self
    }

    /// Set a human-readable description for TUI/documentation use.
    pub fn help(mut self, text: &str) -> Self {
        let t = text.to_string();
        self.modify(|p| p.help = Some(t));
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::ConfigValue;

    #[test]
    fn preset_set_records_overrides() {
        let mut cfg = Config::new();
        cfg.preset("debug")
            .set("CONFIG_FOO", ConfigValue::Bool(true))
            .set("CONFIG_SIZE", ConfigValue::U64(4096));
        let p = cfg.raw.presets.get("debug").unwrap();
        assert_eq!(p.overrides.len(), 2);
        match p.overrides.get("CONFIG_FOO").unwrap() {
            ConfigValue::Bool(true) => {}
            other => panic!("got {other:?}"),
        }
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_preset_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.preset("a").set("X", ConfigValue::Bool(true));
        cfg.preset("a").set("Y", ConfigValue::Bool(false));
        let p = cfg.raw.presets.get("a").unwrap();
        assert_eq!(p.overrides.len(), 1, "second preset's set must NOT mutate first");
        assert_eq!(cfg.diagnostics.len(), 1);
    }

    #[test]
    fn preset_inherits_sets_parent() {
        let mut cfg = Config::new();
        cfg.preset("child").inherits("parent").set("X", ConfigValue::Bool(true));
        let p = cfg.raw.presets.get("child").unwrap();
        assert_eq!(p.inherits.as_deref(), Some("parent"));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn preset_help_sets_text() {
        let mut cfg = Config::new();
        cfg.preset("p").help("some help text");
        let p = cfg.raw.presets.get("p").unwrap();
        assert_eq!(p.help.as_deref(), Some("some help text"));
    }
}
