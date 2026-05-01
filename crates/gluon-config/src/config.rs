use gluon_model::{Handle, ProjectDef, RawBuildModel, TargetDef};
use crate::error::Result;

#[derive(Debug)]
pub struct Config {
    pub(crate) raw: RawBuildModel,
}

impl Config {
    #[must_use]
    pub fn new() -> Self {
        Self { raw: RawBuildModel::default() }
    }

    /// Serialise the underlying `RawBuildModel` as pretty-printed JSON to
    /// stdout. Called from the user's `main()`.
    pub fn emit(&self) -> Result<()> {
        serde_json::to_writer_pretty(std::io::stdout().lock(), &self.raw)
            .map_err(|e| crate::error::ConfigError::new(format!("emit: {e}")))?;
        println!();
        Ok(())
    }

    pub fn project(&mut self, name: &str, version: &str) -> &mut Self {
        self.raw.model.project = Some(ProjectDef {
            name: name.into(),
            version: version.into(),
            ..Default::default()
        });
        self
    }

    /// Insert a builtin rustc target triple. If the same name was already
    /// inserted, return the existing handle without overwriting — Arena::insert
    /// is idempotent on duplicate names.
    pub fn target(&mut self, name: &str) -> Handle<TargetDef> {
        let (h, _) = self.raw.model.targets.insert(name.into(), TargetDef {
            name: name.into(),
            spec: name.into(),
            builtin: true,
            panic_strategy: None,
            span: None,
        });
        h
    }

    /// Insert a custom target backed by a JSON spec file. If the same name was
    /// already inserted, return the existing handle without overwriting.
    pub fn target_spec(&mut self, name: &str, spec: &str) -> Handle<TargetDef> {
        let (h, _) = self.raw.model.targets.insert(name.into(), TargetDef {
            name: name.into(),
            spec: spec.into(),
            builtin: false,
            panic_strategy: None,
            span: None,
        });
        h
    }
}

impl Default for Config {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_config_round_trips_to_empty_raw() {
        let cfg = Config::new();
        let json = serde_json::to_string(&cfg.raw).expect("serde");
        let de: RawBuildModel = serde_json::from_str(&json).expect("de");
        assert!(de.model.project.is_none());
        assert!(de.kconfig_includes.is_empty());
    }
}

#[cfg(test)]
mod project_target_tests {
    use super::*;
    use gluon_model::Handle;

    #[test]
    fn project_sets_name_and_version() {
        let mut cfg = Config::new();
        cfg.project("demo", "0.1.0");
        let p = cfg.raw.model.project.as_ref().expect("project set");
        assert_eq!(p.name, "demo");
        assert_eq!(p.version, "0.1.0");
    }

    #[test]
    fn target_inserts_builtin() {
        let mut cfg = Config::new();
        let h: Handle<gluon_model::TargetDef> = cfg.target("x86_64-unknown-none");
        let t = cfg.raw.model.targets.get(h).unwrap();
        assert_eq!(t.name, "x86_64-unknown-none");
        assert_eq!(t.spec, "x86_64-unknown-none");
        assert!(t.builtin);
    }

    #[test]
    fn target_spec_inserts_custom() {
        let mut cfg = Config::new();
        let h = cfg.target_spec("custom", "targets/custom.json");
        let t = cfg.raw.model.targets.get(h).unwrap();
        assert_eq!(t.name, "custom");
        assert_eq!(t.spec, "targets/custom.json");
        assert!(!t.builtin);
    }

    #[test]
    fn duplicate_target_returns_existing_handle() {
        let mut cfg = Config::new();
        let h1 = cfg.target("foo");
        let h2 = cfg.target("foo");
        assert_eq!(h1, h2, "second target() with same name must return same handle");
        assert_eq!(cfg.raw.model.targets.len(), 1);
    }
}
