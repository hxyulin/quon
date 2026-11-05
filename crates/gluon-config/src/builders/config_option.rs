//! Chainable builder for `ConfigOptionDef` entries.
//!
//! All seven typed `config_*` constructors on [`Config`] funnel through the
//! shared [`ConfigOptionBuilder`]. With `config_options` now an
//! [`Arena<ConfigOptionDef>`], the builder carries a `Handle<ConfigOptionDef>`
//! in its `Valid` state so chained mutations are O(1) instead of O(log n) by
//! name.

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{Binding, ConfigOptionDef, ConfigType, ConfigValue, Expr, Handle};

/// Chainable builder returned by the `config_bool` / `config_u32` / … family.
pub struct ConfigOptionBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<ConfigOptionDef>>,
}

impl<'cfg> ConfigOptionBuilder<'cfg> {
    /// Returns the option's arena handle if the builder is in `Valid` state.
    fn handle(&self) -> Option<Handle<ConfigOptionDef>> {
        match &self.state {
            BuilderState::Valid { state, .. } => Some(*state),
            BuilderState::Duplicate { .. } => None,
        }
    }

    /// Apply `f` to the `ConfigOptionDef` if this builder is not a duplicate.
    fn modify<F: FnOnce(&mut ConfigOptionDef)>(&mut self, f: F) {
        if let Some(h) = self.handle()
            && let Some(opt) = self.cfg.raw.config_options.get_mut(h)
        {
            f(opt);
        }
    }

    /// Override the default value. The value must be type-compatible with the
    /// option's declared `ConfigType`; if it isn't, a diagnostic is pushed and
    /// the default is left unchanged.
    ///
    /// Mirrors the engine's type-mismatch wording verbatim:
    /// `"config option '{name}': expected {type} value, got {observed}"`.
    pub fn default(self, value: ConfigValue) -> Self {
        if let Some(h) = self.handle() {
            let name = self.state.name().to_string();
            if let Some(opt) = self.cfg.raw.config_options.get(h) {
                let ty = opt.ty;
                // Type-compatibility check: mirror the engine's conversions.rs
                // `dynamic_to_config_value` logic. In the Rust API we receive a
                // typed `ConfigValue` directly (not a Rhai Dynamic), so we
                // compare discriminants rather than dynamic types.
                let compatible = matches!(
                    (ty, &value),
                    (ConfigType::Bool, ConfigValue::Bool(_))
                        | (ConfigType::U32, ConfigValue::U32(_))
                        | (ConfigType::U64, ConfigValue::U64(_))
                        | (ConfigType::Str, ConfigValue::Str(_))
                        | (ConfigType::Choice, ConfigValue::Choice(_))
                        | (ConfigType::List, ConfigValue::List(_))
                        | (ConfigType::Tristate, ConfigValue::Tristate(_))
                );

                if compatible {
                    if let Some(opt) = self.cfg.raw.config_options.get_mut(h) {
                        opt.default = value;
                    }
                } else {
                    // Mirror engine wording from conversions.rs:
                    // "config option '{option_name}': expected {expected_str} value, got {observed}"
                    let expected_str = match ty {
                        ConfigType::Bool => "bool",
                        ConfigType::U32 => "u32",
                        ConfigType::U64 => "u64",
                        ConfigType::Str => "string",
                        ConfigType::Choice => "string (choice variant name)",
                        ConfigType::List => "array of strings",
                        ConfigType::Tristate => "tristate string (\"y\"/\"n\"/\"m\")",
                        ConfigType::Group => "group",
                    };
                    let observed = match &value {
                        ConfigValue::Bool(_) => "bool",
                        ConfigValue::U32(_) => "u32",
                        ConfigValue::U64(_) => "u64",
                        ConfigValue::Str(_) => "string",
                        ConfigValue::Choice(_) => "choice string",
                        ConfigValue::List(_) => "array",
                        ConfigValue::Tristate(_) => "tristate",
                    };
                    self.cfg.diagnostics.push(ConfigError::new(format!(
                        "config option '{name}': expected {expected_str} value, got {observed}"
                    )));
                }
            }
        }
        self
    }

    /// Set a human-readable description for TUI/documentation use.
    pub fn help(mut self, text: &str) -> Self {
        let t = text.to_string();
        self.modify(|opt| opt.help = Some(t));
        self
    }

    /// Set a numeric range constraint `[lo, hi]` (inclusive).
    ///
    /// Mirrors the engine's range validation:
    /// - type guard: `"config option '{name}': .range() only applies to numeric (U32/U64) options"`
    /// - invalid range: `"config option '{name}': invalid range ({lo}..={hi})"`
    pub fn range(mut self, lo: u64, hi: u64) -> Self {
        let Some(h) = self.handle() else {
            return self;
        };
        let name = self.state.name().to_string();
        let ty = self.cfg.raw.config_options.get(h).map(|o| o.ty);
        match ty {
            Some(ConfigType::U32) | Some(ConfigType::U64) => {
                if lo > hi {
                    self.cfg.diagnostics.push(ConfigError::new(format!(
                        "config option '{name}': invalid range ({lo}..={hi})"
                    )));
                } else {
                    self.modify(|opt| opt.range = Some((lo, hi)));
                }
            }
            Some(_) => {
                self.cfg.diagnostics.push(ConfigError::new(format!(
                    "config option '{name}': .range() only applies to numeric (U32/U64) options"
                )));
            }
            None => {} // option not present; should not happen on Valid state
        }
        self
    }

    /// Set the exhaustive list of valid variant names.
    ///
    /// Mirrors the engine's type guard:
    /// `"config option '{name}': .choices() only applies to Choice options"`
    pub fn choices(mut self, values: &[&str]) -> Self {
        let Some(h) = self.handle() else {
            return self;
        };
        let name = self.state.name().to_string();
        let ty = self.cfg.raw.config_options.get(h).map(|o| o.ty);
        match ty {
            Some(ConfigType::Choice) => {
                let v: Vec<String> = values.iter().map(|s| s.to_string()).collect();
                self.modify(|opt| opt.choices = Some(v));
            }
            Some(_) => {
                self.cfg.diagnostics.push(ConfigError::new(format!(
                    "config option '{name}': .choices() only applies to Choice options"
                )));
            }
            None => {} // option not present; should not happen on Valid state
        }
        self
    }

    /// Assign this option to a TUI menu category.
    pub fn menu(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|opt| opt.menu = Some(n));
        self
    }

    /// Declare which other options this one force-enables when active.
    pub fn selects(mut self, targets: &[&str]) -> Self {
        let v: Vec<String> = targets.iter().map(|s| s.to_string()).collect();
        self.modify(|opt| opt.selects = v);
        self
    }

    /// Append a code-generation binding.
    pub fn binding(mut self, b: Binding) -> Self {
        self.modify(|opt| opt.bindings.push(b));
        self
    }

    /// Set the `depends_on` clause as a structural [`Expr`].
    ///
    /// The resolver evaluates this expression with full
    /// `&&` / `||` / `!` / grouping support — `Expr::Ident("X")`
    /// means "depends on X"; `Expr::And(vec![..])` composes
    /// requirements; `Expr::Not(..)` negates. See [`Expr::eval`].
    ///
    /// Last-call-wins: a second call overwrites the previous value.
    /// Users who want the AND-fold pattern Rhai exposes through
    /// `.depends_on(name)` chains pass a structural `Expr::And`
    /// in a single call.
    ///
    /// **No string-form parser:** Rhai's
    /// `.depends_on_expr("A && !B")` is unavailable here because the
    /// parser lives in `gluon-core` and would require a circular
    /// dependency. For projects that lean heavily on boolean logic,
    /// declare options in a `.kconfig` file and pull it in via
    /// [`crate::Config::include_kconfig`].
    pub fn depends_on(mut self, expr: Expr) -> Self {
        self.modify(|opt| opt.depends_on_expr = Some(expr));
        self
    }

    /// Alias for [`Self::default`].
    ///
    /// The Rust API exposes both names; see the migration guide in
    /// `docs/src/migration/` for the surface-level mapping from the
    /// old scripted format.
    pub fn default_value(self, value: ConfigValue) -> Self {
        self.default(value)
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::{ConfigOptionDef, ConfigType, ConfigValue};

    /// Test helper: look up a config option by name in the arena.
    fn opt<'a>(cfg: &'a Config, name: &str) -> &'a ConfigOptionDef {
        let h = cfg.raw.config_options.lookup(name).expect("option present");
        cfg.raw.config_options.get(h).expect("handle valid")
    }

    #[test]
    fn config_bool_chain() {
        let mut cfg = Config::new();
        cfg.config_bool("CONFIG_FOO")
            .default(ConfigValue::Bool(true))
            .help("helpful");
        let opt = opt(&cfg, "CONFIG_FOO");
        assert_eq!(opt.ty, ConfigType::Bool);
        match opt.default {
            ConfigValue::Bool(true) => {}
            ref other => panic!("expected Bool(true), got {other:?}"),
        }
        assert_eq!(opt.help.as_deref(), Some("helpful"));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn config_u32_with_range() {
        let mut cfg = Config::new();
        cfg.config_u32("CONFIG_SIZE")
            .default(ConfigValue::U32(4096))
            .range(0, 65536);
        let opt = opt(&cfg, "CONFIG_SIZE");
        assert_eq!(opt.ty, ConfigType::U32);
        assert_eq!(opt.range, Some((0, 65536)));
    }

    #[test]
    fn config_choice_options() {
        let mut cfg = Config::new();
        cfg.config_choice("CONFIG_MODE")
            .choices(&["debug", "release"])
            .default(ConfigValue::Choice("debug".into()));
        let opt = opt(&cfg, "CONFIG_MODE");
        assert_eq!(opt.ty, ConfigType::Choice);
        match &opt.default {
            ConfigValue::Choice(s) if s == "debug" => {}
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn duplicate_config_option_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.config_bool("CONFIG_FOO").help("first");
        cfg.config_bool("CONFIG_FOO").help("second");
        let opt = opt(&cfg, "CONFIG_FOO");
        assert_eq!(
            opt.help.as_deref(),
            Some("first"),
            "first definition preserved"
        );
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("CONFIG_FOO"));
    }

    #[test]
    fn type_mismatched_default_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.config_bool("CONFIG_FOO").default(ConfigValue::U32(42));
        // Engine wording: "config option '{name}': expected bool value, got u32"
        assert!(
            cfg.diagnostics
                .iter()
                .any(|d| { d.message.contains("CONFIG_FOO") && d.message.contains("bool") }),
            "expected type-mismatch diagnostic, got: {:?}",
            cfg.diagnostics,
        );
    }

    #[test]
    fn depends_on_records_compound_expr() {
        use gluon_model::Expr;
        let mut cfg = Config::new();
        cfg.config_bool("FANCY").depends_on(Expr::And(vec![
            Expr::Ident("A".into()),
            Expr::Not(Box::new(Expr::Ident("B".into()))),
        ]));
        let opt = opt(&cfg, "FANCY");
        match opt.depends_on_expr.as_ref() {
            Some(Expr::And(xs)) => {
                assert_eq!(xs.len(), 2);
                assert!(matches!(&xs[0], Expr::Ident(a) if a == "A"));
                assert!(matches!(&xs[1], Expr::Not(_)));
            }
            other => panic!("expected And, got {other:?}"),
        }
    }

    #[test]
    fn depends_on_overwrites_on_repeat_call() {
        // Last-call-wins: `.depends_on(...)` overwrites any previous
        // value. To compose, the user passes a structural `Expr::And`
        // in a single call.
        use gluon_model::Expr;
        let mut cfg = Config::new();
        cfg.config_bool("FANCY")
            .depends_on(Expr::Ident("A".into()))
            .depends_on(Expr::Ident("B".into()));
        let opt = opt(&cfg, "FANCY");
        match opt.depends_on_expr.as_ref() {
            Some(Expr::Ident(s)) if s == "B" => {}
            other => panic!("expected Ident(\"B\"), got {other:?}"),
        }
    }

    #[test]
    fn range_lo_gt_hi_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.config_u32("CONFIG_N").range(100, 0);
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(
            cfg.diagnostics[0].message.contains("invalid range"),
            "got: {}",
            cfg.diagnostics[0].message,
        );
    }

    #[test]
    fn range_on_non_numeric_type_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.config_bool("CONFIG_FOO").range(0, 100);
        assert!(
            cfg.diagnostics
                .iter()
                .any(|d| { d.message.contains("range") && d.message.contains("CONFIG_FOO") })
        );
        let opt = opt(&cfg, "CONFIG_FOO");
        assert!(opt.range.is_none(), "range must NOT be set on Bool option");
    }

    #[test]
    fn choices_on_non_choice_type_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.config_str("CONFIG_NAME").choices(&["a", "b"]);
        assert!(
            cfg.diagnostics
                .iter()
                .any(|d| { d.message.contains("choices") && d.message.contains("CONFIG_NAME") })
        );
        let opt = opt(&cfg, "CONFIG_NAME");
        assert!(
            opt.choices.is_none(),
            "choices must NOT be set on Str option"
        );
    }
}
