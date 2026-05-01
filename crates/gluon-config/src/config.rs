use gluon_model::{Handle, ProjectDef, RawBuildModel, TargetDef};
use crate::error::{ConfigError, Result};

#[derive(Debug)]
pub struct Config {
    pub(crate) raw: RawBuildModel,
    pub(crate) diagnostics: Vec<ConfigError>,
}

impl Config {
    #[must_use]
    pub fn new() -> Self {
        Self {
            raw: RawBuildModel::default(),
            diagnostics: Vec::new(),
        }
    }

    /// Serialise the underlying `RawBuildModel` as pretty-printed JSON to
    /// stdout. Called from the user's `main()`.
    ///
    /// Returns `Err(first)` if any diagnostics were accumulated — after
    /// printing all of them to stderr so the user sees the full picture.
    /// Stage 4 may upgrade to a `ConfigError::Multi` variant; Stage 1 keeps
    /// the surface minimal.
    pub fn emit(&self) -> Result<()> {
        if let Some(first) = self.diagnostics.first() {
            // Print every diagnostic to stderr so the user sees them all,
            // then fail with the first one so the parent process gets a
            // non-zero exit. Stage 1 keeps the surface minimal — Stage 4
            // may upgrade to a typed `ConfigError::Multi(Vec<ConfigError>)`.
            for d in &self.diagnostics {
                eprintln!("{d}");
            }
            return Err(first.clone());
        }
        serde_json::to_writer_pretty(std::io::stdout().lock(), &self.raw)
            .map_err(|e| crate::error::ConfigError::new(format!("emit: {e}")))?;
        println!();
        Ok(())
    }

    pub fn project(&mut self, name: &str, version: &str) -> &mut Self {
        // Last call wins. A `gluon.rs` declares exactly one project; duplicate
        // detection here would just complicate the legitimate "I overwrote it"
        // pattern in test fixtures. The engine path has the same semantic.
        self.raw.model.project = Some(ProjectDef {
            name: name.into(),
            version: version.into(),
            ..Default::default()
        });
        self
    }

    /// Insert a builtin rustc target triple. If the same name was already
    /// inserted, return the existing handle without overwriting — Arena::insert
    /// is idempotent on duplicate names. A diagnostic is accumulated so
    /// `emit()` can surface the error; the handle remains valid for chaining.
    #[must_use = "pass this handle to profile().target() / group().target() / etc."]
    pub fn target(&mut self, name: &str) -> Handle<TargetDef> {
        let (h, inserted) = self.raw.model.targets.insert(name.into(), TargetDef {
            name: name.into(),
            spec: name.into(),
            builtin: true,
            panic_strategy: None,
            span: None,
        });
        if !inserted {
            self.diagnostics.push(ConfigError::new(format!(
                "target \"{name}\" is defined more than once"
            )));
        }
        h
    }

    /// Insert a custom target backed by a JSON spec file. If the same name was
    /// already inserted, return the existing handle without overwriting. A
    /// diagnostic is accumulated so `emit()` can surface the error.
    #[must_use = "pass this handle to profile().target() / group().target() / etc."]
    pub fn target_spec(&mut self, name: &str, spec: &str) -> Handle<TargetDef> {
        let (h, inserted) = self.raw.model.targets.insert(name.into(), TargetDef {
            name: name.into(),
            spec: spec.into(),
            builtin: false,
            panic_strategy: None,
            span: None,
        });
        if !inserted {
            self.diagnostics.push(ConfigError::new(format!(
                "target \"{name}\" is defined more than once"
            )));
        }
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
        assert!(cfg.diagnostics.is_empty(), "fresh Config has no diagnostics");
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

    #[test]
    fn duplicate_target_pushes_diagnostic() {
        let mut cfg = Config::new();
        let _ = cfg.target("foo");
        let _ = cfg.target("foo");
        assert_eq!(cfg.diagnostics.len(), 1, "second target() must push exactly one diagnostic");
        assert!(
            cfg.diagnostics[0].message.contains("foo")
                && cfg.diagnostics[0].message.contains("defined more than once"),
            "expected duplicate-target diagnostic, got: {}",
            cfg.diagnostics[0].message,
        );
    }

    #[test]
    fn duplicate_target_spec_pushes_diagnostic() {
        let mut cfg = Config::new();
        let _ = cfg.target_spec("custom", "a.json");
        let _ = cfg.target_spec("custom", "b.json");
        assert_eq!(cfg.diagnostics.len(), 1);
    }

    #[test]
    fn emit_returns_err_when_diagnostics_present() {
        let mut cfg = Config::new();
        cfg.diagnostics.push(ConfigError::new("boom"));
        let result = cfg.emit();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().message, "boom");
    }
}
