use gluon_model::{BuildStepDef, EspDef, GroupDef, Handle, ImageDef, ProfileDef, ProjectDef, RawBuildModel, RuleDef, RuleHandler, TargetDef};
use crate::builders::{BootloaderBuilder, BuildStepBuilder, BuilderState, EspBuilder, GroupBuilder, ImageBuilder, ProfileBuilder, QemuBuilder, RuleBuilder};
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

    /// Declare a crate group. Returns a chainable [`GroupBuilder`] that
    /// populates the new `GroupDef` and can spawn [`crate::builders::CrateBuilder`]s
    /// via [`GroupBuilder::add`].
    ///
    /// If a group with this name was already declared, the builder enters the
    /// `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    pub fn group(&mut self, name: &str) -> GroupBuilder<'_> {
        let (handle, inserted) = self.raw.model.groups.insert(
            name.into(),
            GroupDef { name: name.into(), ..Default::default() },
        );
        let state = if inserted {
            BuilderState::Valid { state: handle, name: name.into(), span: None }
        } else {
            // Mirror the engine's exact diagnostic wording so the user sees
            // the same message regardless of whether they use the DSL or the
            // Rust config API.
            self.diagnostics.push(ConfigError::new(format!(
                "group \"{name}\" is defined more than once"
            )));
            BuilderState::Duplicate { name: name.into(), span: None }
        };
        GroupBuilder { cfg: self, state }
    }

    /// Declare a disk image. Returns a chainable [`ImageBuilder`] that
    /// populates the new [`ImageDef`] and can spawn [`PartitionBuilder`]s
    /// via [`ImageBuilder::partition`].
    ///
    /// If an image with this name was already declared, the builder enters
    /// the `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    ///
    /// Mirrors the engine's exact diagnostic string:
    /// `"image(\"{name}\") is defined more than once"`.
    pub fn image(&mut self, name: &str) -> ImageBuilder<'_> {
        let (handle, inserted) = self.raw.model.images.insert(
            name.into(),
            ImageDef { name: name.into(), ..Default::default() },
        );
        let state = if inserted {
            BuilderState::Valid { state: handle, name: name.into(), span: None }
        } else {
            // Mirror the engine's exact diagnostic wording so the user sees
            // the same message regardless of whether they use the DSL or the
            // Rust config API.
            self.diagnostics.push(ConfigError::new(format!(
                "image(\"{name}\") is defined more than once"
            )));
            BuilderState::Duplicate { name: name.into(), span: None }
        };
        ImageBuilder { cfg: self, state }
    }

    /// Declare a build profile. Returns a chainable [`ProfileBuilder`] that
    /// populates the new `ProfileDef`.
    ///
    /// If a profile with this name was already declared, the builder enters
    /// the `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    pub fn profile(&mut self, name: &str) -> ProfileBuilder<'_> {
        let (handle, inserted) = self.raw.model.profiles.insert(
            name.into(),
            ProfileDef { name: name.into(), ..Default::default() },
        );
        let state = if inserted {
            BuilderState::Valid { state: handle, name: name.into(), span: None }
        } else {
            // Mirror the engine's exact diagnostic wording so the user sees
            // the same message regardless of whether they use the DSL or the
            // Rust config API.
            //
            // The handle returned here is the existing entry's handle, but we
            // deliberately do NOT store it in the Valid branch: chained methods
            // on a Duplicate must be no-ops so the first definition is preserved.
            self.diagnostics.push(ConfigError::new(format!(
                "profile \"{name}\" is defined more than once"
            )));
            BuilderState::Duplicate { name: name.into(), span: None }
        };
        ProfileBuilder { cfg: self, state }
    }

    /// Configure the singleton QEMU runner. Returns a chainable
    /// [`QemuBuilder`] that populates the project-level `QemuDef`.
    ///
    /// `qemu` is a **singleton**: calling this more than once pushes a
    /// [`ConfigError`] into `self.diagnostics` and returns a builder in the
    /// `Duplicate` state — all chained methods on the second (and subsequent)
    /// builder become no-ops so the first call's values are preserved.
    ///
    /// This mirrors the engine's exact diagnostic string:
    /// `"qemu is defined more than once"`.
    pub fn qemu(&mut self, program: &str) -> QemuBuilder<'_> {
        let state = if self.raw.qemu_was_set {
            // Mirror engine wording verbatim.
            self.diagnostics
                .push(ConfigError::new("qemu is defined more than once".to_string()));
            BuilderState::Duplicate { name: program.into(), span: None }
        } else {
            self.raw.qemu_was_set = true;
            self.raw.model.qemu.binary = Some(program.into());
            BuilderState::Valid { state: (), name: program.into(), span: None }
        };
        QemuBuilder { cfg: self, state }
    }

    /// Configure the singleton bootloader. Returns a chainable
    /// [`BootloaderBuilder`] that populates the project-level `BootloaderDef`.
    ///
    /// `bootloader` is a **singleton**: calling this more than once pushes a
    /// [`ConfigError`] into `self.diagnostics` and returns a builder in the
    /// `Duplicate` state — all chained methods on the second (and subsequent)
    /// builder become no-ops so the first call's values are preserved.
    ///
    /// This mirrors the engine's exact diagnostic string:
    /// `"bootloader is defined more than once"`.
    pub fn bootloader(&mut self, kind: &str) -> BootloaderBuilder<'_> {
        let state = if self.raw.bootloader_was_set {
            // Mirror engine wording verbatim.
            self.diagnostics
                .push(ConfigError::new("bootloader is defined more than once".to_string()));
            BuilderState::Duplicate { name: kind.into(), span: None }
        } else {
            self.raw.bootloader_was_set = true;
            self.raw.model.bootloader.kind = kind.into();
            BuilderState::Valid { state: (), name: kind.into(), span: None }
        };
        BootloaderBuilder { cfg: self, state }
    }

    /// Declare a build rule. Returns a chainable [`RuleBuilder`] that
    /// populates the new `RuleDef`.
    ///
    /// If a rule with this name was already declared, the builder enters the
    /// `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    ///
    /// Mirrors the engine's exact diagnostic string:
    /// `"rule '{name}' is defined more than once"` (single quotes, engine line 166).
    pub fn rule(&mut self, name: &str) -> RuleBuilder<'_> {
        let (handle, inserted) = self.raw.model.rules.insert(
            name.into(),
            RuleDef {
                name: name.into(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                depends_on: Vec::new(),
                handler: RuleHandler::Builtin("exec".into()),
                working_dir: None,
                span: None,
            },
        );
        let state = if inserted {
            BuilderState::Valid { state: handle, name: name.into(), span: None }
        } else {
            // Mirror the engine's exact diagnostic wording (single quotes, engine line 166).
            self.diagnostics.push(ConfigError::new(format!(
                "rule '{name}' is defined more than once"
            )));
            BuilderState::Duplicate { name: name.into(), span: None }
        };
        RuleBuilder { cfg: self, state }
    }

    /// Declare a build step. Returns a chainable [`BuildStepBuilder`] that
    /// populates the new `BuildStepDef`.
    ///
    /// If a build step with this name was already declared, the builder enters
    /// the `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    ///
    /// If the name collides with an existing crate, a diagnostic is pushed
    /// (mirroring the engine validator in `validate.rs`) but the build step
    /// is still inserted — the validator, not the builder, is where this is
    /// enforced at engine level.
    ///
    /// Mirrors the engine's exact diagnostic string:
    /// `"build_step(\"{name}\") is defined more than once"`.
    pub fn build_step(&mut self, name: &str) -> BuildStepBuilder<'_> {
        // Check for crate-name collision before insert, mirroring the engine
        // validator (validate.rs:103). We still insert regardless — the
        // validator does too.
        if self.raw.model.crates.lookup(name).is_some() {
            self.diagnostics.push(ConfigError::new(format!(
                "build_step(\"{name}\") collides with a crate of the same name. \
                 Pick a different build-step name; the unified artifact \
                 namespace requires unique names."
            )));
        }

        let (handle, inserted) = self.raw.model.build_steps.insert(
            name.into(),
            BuildStepDef { name: name.into(), ..Default::default() },
        );
        let state = if inserted {
            BuilderState::Valid { state: handle, name: name.into(), span: None }
        } else {
            // Mirror the engine's exact diagnostic wording.
            self.diagnostics.push(ConfigError::new(format!(
                "build_step(\"{name}\") is defined more than once"
            )));
            BuilderState::Duplicate { name: name.into(), span: None }
        };
        BuildStepBuilder { cfg: self, state }
    }

    /// Declare an EFI System Partition layout. Returns a chainable
    /// [`EspBuilder`] that accumulates `(source_crate, dest_path)` entries.
    ///
    /// If an ESP with this name was already declared, the builder enters the
    /// `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    ///
    /// Mirrors the engine's exact diagnostic string:
    /// `"esp(\"{name}\") is defined more than once"`.
    pub fn esp(&mut self, name: &str) -> EspBuilder<'_> {
        let (handle, inserted) = self.raw.model.esps.insert(
            name.into(),
            EspDef { name: name.into(), entries: Vec::new(), span: None },
        );
        let state = if inserted {
            BuilderState::Valid { state: handle, name: name.into(), span: None }
        } else {
            // Mirror the engine's exact diagnostic wording.
            self.diagnostics.push(ConfigError::new(format!(
                "esp(\"{name}\") is defined more than once"
            )));
            BuilderState::Duplicate { name: name.into(), span: None }
        };
        EspBuilder { cfg: self, state }
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
