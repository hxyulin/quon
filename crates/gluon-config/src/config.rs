use crate::builders::{
    BootloaderBuilder, BuildStepBuilder, BuilderState, ConfigOptionBuilder, EspBuilder,
    GroupBuilder, ImageBuilder, PresetBuilder, ProfileBuilder, ProjectBuilder, QemuBuilder,
    RuleBuilder,
};
use crate::error::{ConfigError, Result};
use gluon_model::{
    BootloaderKind, ConfigOptionDef, ConfigType, ConfigValue, Handle, ImageDef, PresetDef,
    ProjectDef, QemuDef, RawBootloaderDef, RawBuildModel, RawBuildStepDef, RawEspDef, RawGroupDef,
    RawProfileDef, RuleDef, RuleHandler, TargetDef, TristateVal,
};

#[derive(Debug)]
pub struct Config {
    pub(crate) raw: RawBuildModel,
    pub(crate) diagnostics: Vec<ConfigError>,
}

/// Render a `TargetDef` in human-readable form for redefinition
/// diagnostics. Used by `Config::target` and `Config::target_spec` to
/// describe the *previous* shape of a target when a real conflict is
/// detected, so the user can see both the old and new declaration.
fn describe_target(t: &TargetDef) -> String {
    if t.builtin {
        format!("builtin target \"{}\"", t.spec)
    } else {
        format!("custom spec \"{}\"", t.spec)
    }
}

impl Config {
    #[must_use]
    pub fn new() -> Self {
        Self {
            raw: RawBuildModel::default(),
            diagnostics: Vec::new(),
        }
    }

    /// Borrow the underlying `RawBuildModel`. Useful for tests and
    /// in-process tooling that wants to drive `gluon_model::intern`
    /// without round-tripping through stdout JSON.
    ///
    /// The returned reference reflects every builder call made so
    /// far. Diagnostics accumulated by builders are reachable via
    /// [`Config::diagnostics`] (see below).
    pub fn raw(&self) -> &gluon_model::RawBuildModel {
        &self.raw
    }

    /// Consume the builder and return the underlying `RawBuildModel`.
    /// Mirrors [`Config::raw`] but transfers ownership — convenient
    /// when the caller wants to feed the raw model into intern
    /// without keeping the builder alive.
    pub fn into_raw(self) -> gluon_model::RawBuildModel {
        self.raw
    }

    /// Borrow the accumulated diagnostics. Empty unless a builder
    /// flagged a duplicate name or other recoverable error.
    pub fn diagnostics(&self) -> &[crate::error::ConfigError] {
        &self.diagnostics
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

    pub fn project(&mut self, name: &str, version: &str) -> ProjectBuilder<'_> {
        // Last call wins — no duplicate detection. A `gluon.rs` declares
        // exactly one project; duplicate detection here would just
        // complicate the legitimate "I overwrote it" pattern in test
        // fixtures. The engine path has the same semantic.
        self.raw.project = Some(ProjectDef {
            name: name.into(),
            version: version.into(),
            ..Default::default()
        });
        ProjectBuilder {
            cfg: self,
            state: BuilderState::Valid {
                state: (),
                name: name.into(),
                span: None,
            },
        }
    }

    /// Insert a builtin rustc target triple, returning a handle for use
    /// with `profile().target()` / `group().target()` / etc.
    ///
    /// Idempotent on identical re-declarations: calling
    /// `cfg.target("x86_64-unknown-none")` from multiple subsystems
    /// returns the same handle and accumulates no diagnostic. A
    /// diagnostic is only pushed if the second call would *change* the
    /// existing entry (e.g. the same name was previously inserted via
    /// `target_spec` with a JSON path, or with a different builtin
    /// spec). This idempotence is the foundation of the multi-file
    /// `gluon.rs` pattern documented in
    /// `docs/src/configuration/gluon-rs.md`.
    #[must_use = "pass this handle to profile().target() / group().target() / etc."]
    pub fn target(&mut self, name: &str) -> Handle<TargetDef> {
        let candidate = TargetDef {
            name: name.into(),
            spec: name.into(),
            builtin: true,
            panic_strategy: None,
            span: None,
        };
        let (h, inserted) = self.raw.targets.insert(name.into(), candidate);
        if !inserted {
            let existing = self.raw.targets.get(h).expect("handle just returned");
            // Identity = (spec, builtin) only. Fields that may be set
            // by later chained builders (panic_strategy, span) are
            // excluded so legitimate re-lookups stay silent. If a
            // setter lands for one of those fields, update this
            // predicate to fold it in — otherwise conflicting later
            // setters would silently agree.
            if existing.spec != name || !existing.builtin {
                self.diagnostics.push(ConfigError::new(format!(
                    "target \"{name}\" is redefined: previously {prev}, now builtin target \"{name}\"",
                    prev = describe_target(existing),
                )));
            }
        }
        h
    }

    /// Insert a custom target backed by a JSON spec file, returning a
    /// handle for use with `profile().target()` / `group().target()` / etc.
    ///
    /// Idempotent on identical re-declarations (same name AND same
    /// spec path). A diagnostic is pushed if the second call would
    /// *change* the existing entry — e.g. a different spec path, or
    /// the same name was previously inserted via `target()` as a
    /// builtin.
    #[must_use = "pass this handle to profile().target() / group().target() / etc."]
    pub fn target_spec(&mut self, name: &str, spec: &str) -> Handle<TargetDef> {
        let candidate = TargetDef {
            name: name.into(),
            spec: spec.into(),
            builtin: false,
            panic_strategy: None,
            span: None,
        };
        let (h, inserted) = self.raw.targets.insert(name.into(), candidate);
        if !inserted {
            let existing = self.raw.targets.get(h).expect("handle just returned");
            // Identity rule mirrors `target()` above.
            if existing.spec != spec || existing.builtin {
                self.diagnostics.push(ConfigError::new(format!(
                    "target \"{name}\" is redefined: previously {prev}, now custom spec \"{spec}\"",
                    prev = describe_target(existing),
                )));
            }
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
        let (handle, inserted) = self.raw.groups.insert(
            name.into(),
            RawGroupDef {
                name: name.into(),
                ..Default::default()
            },
        );
        let state = if inserted {
            BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            }
        } else {
            // Mirror the engine's exact diagnostic wording so the user sees
            // the same message regardless of whether they use the DSL or the
            // Rust config API.
            self.diagnostics.push(ConfigError::new(format!(
                "group \"{name}\" is defined more than once"
            )));
            BuilderState::Duplicate {
                name: name.into(),
                span: None,
            }
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
        let (handle, inserted) = self.raw.images.insert(
            name.into(),
            ImageDef {
                name: name.into(),
                ..Default::default()
            },
        );
        let state = if inserted {
            BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            }
        } else {
            // Mirror the engine's exact diagnostic wording so the user sees
            // the same message regardless of whether they use the DSL or the
            // Rust config API.
            self.diagnostics.push(ConfigError::new(format!(
                "image(\"{name}\") is defined more than once"
            )));
            BuilderState::Duplicate {
                name: name.into(),
                span: None,
            }
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
        let (handle, inserted) = self.raw.profiles.insert(
            name.into(),
            RawProfileDef {
                name: name.into(),
                ..Default::default()
            },
        );
        let state = if inserted {
            BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            }
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
            BuilderState::Duplicate {
                name: name.into(),
                span: None,
            }
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
        let state = if self.raw.qemu.is_some() {
            // Mirror engine wording verbatim.
            self.diagnostics.push(ConfigError::new(
                "qemu is defined more than once".to_string(),
            ));
            BuilderState::Duplicate {
                name: program.into(),
                span: None,
            }
        } else {
            // Allocate the `Some(QemuDef)` payload here so the chained
            // builder methods can `.as_mut().unwrap()` safely (only the
            // `Valid` arm reaches them).
            self.raw.qemu = Some(QemuDef {
                binary: Some(program.into()),
                ..Default::default()
            });
            BuilderState::Valid {
                state: (),
                name: program.into(),
                span: None,
            }
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
        let state = if self.raw.bootloader.is_some() {
            // Mirror engine wording verbatim.
            self.diagnostics.push(ConfigError::new(
                "bootloader is defined more than once".to_string(),
            ));
            BuilderState::Duplicate {
                name: kind.into(),
                span: None,
            }
        } else {
            // Typed wire format: parse the user's &str into BootloaderKind
            // here so the raw model and the resolved model stay symmetric.
            // Unknown values flow through as Custom(s) (still useful — e.g.
            // limine). Allocating the `Some(...)` payload here means the
            // builder's chained methods can `.as_mut().unwrap()` safely
            // (see `BootloaderBuilder::modify`).
            self.raw.bootloader = Some(RawBootloaderDef {
                kind: BootloaderKind::from_str(kind),
                ..Default::default()
            });
            BuilderState::Valid {
                state: (),
                name: kind.into(),
                span: None,
            }
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
        let (handle, inserted) = self.raw.rules.insert(
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
            BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            }
        } else {
            // Mirror the engine's exact diagnostic wording (single quotes, engine line 166).
            self.diagnostics.push(ConfigError::new(format!(
                "rule '{name}' is defined more than once"
            )));
            BuilderState::Duplicate {
                name: name.into(),
                span: None,
            }
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
        if self.raw.crates.lookup(name).is_some() {
            self.diagnostics.push(ConfigError::new(format!(
                "build_step(\"{name}\") collides with a crate of the same name. \
                 Pick a different build-step name; the unified artifact \
                 namespace requires unique names."
            )));
        }

        let (handle, inserted) = self.raw.build_steps.insert(
            name.into(),
            RawBuildStepDef {
                name: name.into(),
                ..Default::default()
            },
        );
        let state = if inserted {
            BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            }
        } else {
            // Mirror the engine's exact diagnostic wording.
            self.diagnostics.push(ConfigError::new(format!(
                "build_step(\"{name}\") is defined more than once"
            )));
            BuilderState::Duplicate {
                name: name.into(),
                span: None,
            }
        };
        BuildStepBuilder { cfg: self, state }
    }

    /// Declare an EFI System Partition layout. Returns a chainable
    /// [`EspBuilder`] that accumulates `(source, dest_path)` entries.
    ///
    /// If an ESP with this name was already declared, the builder enters the
    /// `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    ///
    /// Mirrors the engine's exact diagnostic string:
    /// `"esp(\"{name}\") is defined more than once"`.
    pub fn esp(&mut self, name: &str) -> EspBuilder<'_> {
        let (handle, inserted) = self.raw.esps.insert(
            name.into(),
            RawEspDef {
                name: name.into(),
                entries: Vec::new(),
                span: None,
            },
        );
        let state = if inserted {
            BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            }
        } else {
            // Mirror the engine's exact diagnostic wording.
            self.diagnostics.push(ConfigError::new(format!(
                "esp(\"{name}\") is defined more than once"
            )));
            BuilderState::Duplicate {
                name: name.into(),
                span: None,
            }
        };
        EspBuilder { cfg: self, state }
    }

    // -----------------------------------------------------------------------
    // Config-option builders
    // -----------------------------------------------------------------------

    /// Shared constructor for all typed `config_*` methods.
    ///
    /// If `name` is already registered, a duplicate diagnostic is pushed and a
    /// [`ConfigOptionBuilder`] in the `Duplicate` state is returned so chained
    /// calls become no-ops.  Mirrors the engine's exact wording:
    /// `"config option '{name}' is defined more than once"`.
    fn config_option_with_type(
        &mut self,
        name: &str,
        ty: ConfigType,
        default: ConfigValue,
    ) -> ConfigOptionBuilder<'_> {
        let (handle, inserted) = self.raw.config_options.insert(
            name.into(),
            ConfigOptionDef {
                name: name.into(),
                ty,
                default,
                help: None,
                selects: Vec::new(),
                range: None,
                choices: None,
                menu: None,
                bindings: Vec::new(),
                span: None,
                depends_on_expr: None,
                visible_if_expr: None,
            },
        );
        if !inserted {
            self.diagnostics.push(ConfigError::new(format!(
                "config option '{name}' is defined more than once"
            )));
            return ConfigOptionBuilder {
                cfg: self,
                state: BuilderState::Duplicate {
                    name: name.into(),
                    span: None,
                },
            };
        }
        ConfigOptionBuilder {
            cfg: self,
            state: BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            },
        }
    }

    /// Declare a boolean config option (default: `false`).
    pub fn config_bool(&mut self, name: &str) -> ConfigOptionBuilder<'_> {
        self.config_option_with_type(name, ConfigType::Bool, ConfigValue::Bool(false))
    }

    /// Declare a tristate config option (default: `No`).
    pub fn config_tristate(&mut self, name: &str) -> ConfigOptionBuilder<'_> {
        self.config_option_with_type(
            name,
            ConfigType::Tristate,
            ConfigValue::Tristate(TristateVal::No),
        )
    }

    /// Declare a 32-bit unsigned integer config option (default: `0`).
    pub fn config_u32(&mut self, name: &str) -> ConfigOptionBuilder<'_> {
        self.config_option_with_type(name, ConfigType::U32, ConfigValue::U32(0))
    }

    /// Declare a 64-bit unsigned integer config option (default: `0`).
    pub fn config_u64(&mut self, name: &str) -> ConfigOptionBuilder<'_> {
        self.config_option_with_type(name, ConfigType::U64, ConfigValue::U64(0))
    }

    /// Declare a string config option (default: `""`).
    pub fn config_str(&mut self, name: &str) -> ConfigOptionBuilder<'_> {
        self.config_option_with_type(name, ConfigType::Str, ConfigValue::Str(String::new()))
    }

    /// Declare a choice config option (default: `""`; use `.choices()` to
    /// enumerate the valid variants and `.default(ConfigValue::Choice(...))` to
    /// pick the initial value).
    pub fn config_choice(&mut self, name: &str) -> ConfigOptionBuilder<'_> {
        self.config_option_with_type(name, ConfigType::Choice, ConfigValue::Choice(String::new()))
    }

    /// Declare a list-of-strings config option (default: empty list).
    pub fn config_list(&mut self, name: &str) -> ConfigOptionBuilder<'_> {
        self.config_option_with_type(name, ConfigType::List, ConfigValue::List(Vec::new()))
    }

    // -----------------------------------------------------------------------
    // Preset builder
    // -----------------------------------------------------------------------

    /// Declare a named configuration preset. Returns a chainable
    /// [`PresetBuilder`] that populates the new `PresetDef`.
    ///
    /// If a preset with this name was already declared, the builder enters the
    /// `Duplicate` state: all chained methods become no-ops and the first
    /// definition is preserved. A [`ConfigError`] is pushed into
    /// `self.diagnostics` so [`Config::emit`] can surface the problem.
    ///
    /// Mirrors the engine's exact diagnostic string:
    /// `"preset '{name}' is defined more than once"`.
    pub fn preset(&mut self, name: &str) -> PresetBuilder<'_> {
        if self.raw.presets.contains_key(name) {
            self.diagnostics.push(ConfigError::new(format!(
                "preset '{name}' is defined more than once"
            )));
            return PresetBuilder {
                cfg: self,
                state: BuilderState::Duplicate {
                    name: name.into(),
                    span: None,
                },
            };
        }
        self.raw.presets.insert(
            name.into(),
            PresetDef {
                name: name.into(),
                ..Default::default()
            },
        );
        PresetBuilder {
            cfg: self,
            state: BuilderState::Valid {
                state: (),
                name: name.into(),
                span: None,
            },
        }
    }

    // -----------------------------------------------------------------------
    // Kconfig include
    // -----------------------------------------------------------------------

    /// Declare an external dependency. Returns a chainable
    /// [`DependencyBuilder`] that selects a source
    /// (`.version`/`.path`/`.git`) and configures features/flags.
    ///
    /// Mirrors the engine's exact diagnostic string:
    /// `"dependency \"{name}\" is defined more than once"`.
    pub fn dependency(&mut self, name: &str) -> crate::builders::DependencyBuilder<'_> {
        let (handle, inserted) = self.raw.external_deps.insert(
            name.into(),
            gluon_model::ExternalDepDef {
                name: name.into(),
                source: gluon_model::DepSource::CratesIo {
                    version: String::new(),
                },
                features: Vec::new(),
                default_features: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                target: gluon_model::TargetSpec::Host,
                span: None,
            },
        );
        let state = if inserted {
            crate::builders::BuilderState::Valid {
                state: handle,
                name: name.into(),
                span: None,
            }
        } else {
            self.diagnostics
                .push(crate::error::ConfigError::new(format!(
                    "dependency \"{name}\" is defined more than once"
                )));
            crate::builders::BuilderState::Duplicate {
                name: name.into(),
                span: None,
            }
        };
        crate::builders::DependencyBuilder { cfg: self, state }
    }

    /// Name the Cargo workspace whose packages `group.package(...)`
    /// builds. `dir` holds the workspace `Cargo.toml` and is relative
    /// to the project root. At most one per project.
    ///
    /// Like [`Self::include_kconfig`], nothing is read here: Gluon reads
    /// the manifests on every run, after `gluon.rs` has run, so editing
    /// a `Cargo.toml` does not recompile `gluon.rs`.
    ///
    /// Registry dependencies come from Cargo's registry cache unless the
    /// returned builder names a vendor directory with
    /// [`crate::builders::CargoWorkspaceBuilder::vendored`].
    pub fn cargo_workspace(&mut self, dir: &str) -> crate::builders::CargoWorkspaceBuilder<'_> {
        let active = if let Some(prev) = &self.raw.cargo_workspace {
            self.diagnostics
                .push(crate::error::ConfigError::new(format!(
                    "cargo_workspace is declared more than once (\"{}\" and \"{dir}\")",
                    prev.dir
                )));
            false
        } else {
            self.raw.cargo_workspace = Some(gluon_model::RawCargoWorkspace {
                dir: dir.into(),
                vendored: None,
            });
            true
        };
        crate::builders::CargoWorkspaceBuilder { cfg: self, active }
    }

    /// Record a `.kconfig` file path for host-side parsing.
    ///
    /// The file is **not parsed here** — `gluon-config` cannot depend on
    /// `gluon-core` (would be cyclic).  The `script_runner` (Task 11) reads
    /// each recorded path through `gluon_core::kconfig::load_kconfig` after
    /// deserialising the JSON and before running intern, then merges the
    /// resulting options/presets into the `BuildModel` with the same
    /// duplicate-detection the Rhai engine uses today.
    ///
    /// The `Result<()>` signature is for forward compatibility: Stage 4+ may
    /// validate path existence in-process.
    pub fn include_kconfig(&mut self, path: impl AsRef<std::path::Path>) -> Result<()> {
        self.raw.kconfig_includes.push(path.as_ref().to_path_buf());
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_workspace_vendored_records_the_directory() {
        let mut cfg = Config::new();
        cfg.cargo_workspace(".").vendored("vendor");
        // A repeated declaration does not overwrite the first one's
        // vendor directory.
        cfg.cargo_workspace("other").vendored("elsewhere");
        let ws = cfg.raw.cargo_workspace.as_ref().unwrap();
        assert_eq!(ws.dir, ".");
        assert_eq!(ws.vendored.as_deref(), Some("vendor"));
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(
            cfg.diagnostics[0]
                .message
                .contains("cargo_workspace is declared more than once")
        );
    }
}

#[cfg(test)]
mod project_target_tests {
    use super::*;
    use gluon_model::Handle;

    #[test]
    fn target_inserts_builtin() {
        let mut cfg = Config::new();
        let h: Handle<gluon_model::TargetDef> = cfg.target("x86_64-unknown-none");
        let t = cfg.raw.targets.get(h).unwrap();
        assert_eq!(t.name, "x86_64-unknown-none");
        assert_eq!(t.spec, "x86_64-unknown-none");
        assert!(t.builtin);
    }

    #[test]
    fn target_spec_inserts_custom() {
        let mut cfg = Config::new();
        let h = cfg.target_spec("custom", "targets/custom.json");
        let t = cfg.raw.targets.get(h).unwrap();
        assert_eq!(t.name, "custom");
        assert_eq!(t.spec, "targets/custom.json");
        assert!(!t.builtin);
    }

    #[test]
    fn target_is_idempotent_for_identical_calls() {
        let mut cfg = Config::new();
        let h1 = cfg.target("foo");
        let h2 = cfg.target("foo");
        assert_eq!(h1, h2, "second target() with same name returns same handle");
        assert_eq!(cfg.raw.targets.len(), 1, "no duplicate entry");
        assert!(
            cfg.diagnostics.is_empty(),
            "identical re-declaration must not push a diagnostic; got: {:?}",
            cfg.diagnostics,
        );
    }

    #[test]
    fn target_spec_is_idempotent_for_identical_path() {
        let mut cfg = Config::new();
        let h1 = cfg.target_spec("custom", "targets/custom.json");
        let h2 = cfg.target_spec("custom", "targets/custom.json");
        assert_eq!(h1, h2);
        assert!(
            cfg.diagnostics.is_empty(),
            "identical target_spec re-declaration must not push a diagnostic; got: {:?}",
            cfg.diagnostics,
        );
    }

    #[test]
    fn target_spec_emits_diagnostic_when_path_changes() {
        let mut cfg = Config::new();
        let _ = cfg.target_spec("custom", "a.json");
        let _ = cfg.target_spec("custom", "b.json");
        assert_eq!(
            cfg.diagnostics.len(),
            1,
            "conflicting spec must push diagnostic"
        );
        let msg = &cfg.diagnostics[0].message;
        assert!(
            msg.contains("custom") && msg.contains("a.json") && msg.contains("b.json"),
            "diagnostic should name the target and both paths; got: {msg}"
        );
    }

    #[test]
    fn target_then_target_spec_emits_diagnostic() {
        let mut cfg = Config::new();
        let _ = cfg.target("foo");
        let _ = cfg.target_spec("foo", "foo.json");
        assert_eq!(
            cfg.diagnostics.len(),
            1,
            "switching from builtin to custom spec must push diagnostic"
        );
        let msg = &cfg.diagnostics[0].message;
        assert!(
            msg.contains("foo") && (msg.contains("builtin") || msg.contains("custom")),
            "diagnostic should distinguish builtin vs custom; got: {msg}"
        );
    }

    #[test]
    fn target_spec_then_target_emits_diagnostic() {
        let mut cfg = Config::new();
        let _ = cfg.target_spec("foo", "foo.json");
        let _ = cfg.target("foo");
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
