//! Chainable builder for `BuildStepDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{BuildStepDef, BuildStepInput, Handle};

/// Reserved env-var name used by the gluon runtime to communicate the
/// absolute output path to a build-step runner. Mirrors
/// `gluon_core::engine::builders::pipeline::RESERVED_OUTPUT_ENV`.
const RESERVED_OUTPUT_ENV: &str = "GLUON_OUTPUT_PATH";

/// Chainable builder returned by [`Config::build_step`].
pub struct BuildStepBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<BuildStepDef>>,
}

impl<'cfg> BuildStepBuilder<'cfg> {
    /// Apply `f` to the `BuildStepDef` if this builder is `Valid`.
    /// Uses the captured handle directly — no per-call name lookup or string
    /// allocation.
    fn modify<F: FnOnce(&mut BuildStepDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(b) = self.cfg.raw.model.build_steps.get_mut(*handle) {
                f(b);
            }
        }
    }

    /// Set the runner crate name for this build step. The `runner_handle` is
    /// populated later by the intern pass.
    pub fn runner(mut self, crate_name: &str) -> Self {
        let n = crate_name.to_string();
        self.modify(|b| b.runner = n);
        self
    }

    /// Declare a crate-artifact input. `env_var` is the env-var key passed to
    /// the runner; `crate_name` is resolved to an artifact path at invocation.
    ///
    /// Rejects the reserved env-var name `GLUON_OUTPUT_PATH` with a
    /// diagnostic; the input is not inserted in that case.
    pub fn input_crate(mut self, env_var: &str, crate_name: &str) -> Self {
        if self.check_reserved_input(env_var) {
            return self;
        }
        let key = env_var.to_string();
        let val = BuildStepInput::Crate { crate_name: crate_name.into(), crate_handle: None };
        self.modify(|b| { b.inputs.insert(key, val); });
        self
    }

    /// Declare a project-file input. `env_var` is the env-var key; `path` is
    /// resolved relative to `project_root` at invocation.
    ///
    /// Rejects the reserved env-var name `GLUON_OUTPUT_PATH` with a
    /// diagnostic.
    pub fn input_file(mut self, env_var: &str, path: &str) -> Self {
        if self.check_reserved_input(env_var) {
            return self;
        }
        let key = env_var.to_string();
        let val = BuildStepInput::File { path: path.into() };
        self.modify(|b| { b.inputs.insert(key, val); });
        self
    }

    /// Declare a literal-string input. `env_var` is the env-var key; `value`
    /// is passed verbatim to the runner.
    ///
    /// Rejects the reserved env-var name `GLUON_OUTPUT_PATH` with a
    /// diagnostic.
    pub fn input_literal(mut self, env_var: &str, value: &str) -> Self {
        if self.check_reserved_input(env_var) {
            return self;
        }
        let key = env_var.to_string();
        let val = BuildStepInput::Literal { value: value.into() };
        self.modify(|b| { b.inputs.insert(key, val); });
        self
    }

    /// Declare a literal pass-through env var. Distinct from `input_*` —
    /// env entries are literal strings, not artifact paths.
    ///
    /// Rejects the reserved env-var name `GLUON_OUTPUT_PATH` with a
    /// diagnostic; the entry is not inserted in that case.
    pub fn env(mut self, name: &str, value: &str) -> Self {
        if self.state.is_duplicate() {
            return self;
        }
        if name == RESERVED_OUTPUT_ENV {
            // Mirror engine wording from register_build_step (env method).
            let step_name = self.state.name().to_string();
            self.cfg.diagnostics.push(ConfigError::new(format!(
                "build_step(\"{step_name}\").env: env-var name '{RESERVED_OUTPUT_ENV}' \
                 is reserved for the output path injection — use a different name"
            )));
            return self;
        }
        let k = name.to_string();
        let v = value.to_string();
        self.modify(|b| { b.env.insert(k, v); });
        self
    }

    /// Set the output filename. Plain filename only — path separators are
    /// rejected so the output stays contained in the per-step build directory.
    ///
    /// Mirrors the engine's check: both `/` and `\` are rejected with engine-
    /// parity wording.
    pub fn output(mut self, filename: &str) -> Self {
        if self.state.is_duplicate() {
            return self;
        }
        if filename.contains('/') || filename.contains('\\') {
            let step_name = self.state.name().to_string();
            self.cfg.diagnostics.push(ConfigError::new(format!(
                "build_step(\"{step_name}\").output: filename '{filename}' must not \
                 contain path separators (use a plain filename; gluon places it \
                 under build/build-steps/<name>/)"
            )));
            return self;
        }
        let f = filename.to_string();
        self.modify(|b| b.output = f);
        self
    }

    /// Check whether `env_var` is the reserved output env-var name. If so,
    /// push an engine-parity diagnostic and return `true` (meaning "reject").
    ///
    /// Also returns `true` (silently) when the builder is in `Duplicate`
    /// state, so callers can unconditionally `return self` on a `true` result.
    fn check_reserved_input(&mut self, env_var: &str) -> bool {
        if self.state.is_duplicate() {
            return true;
        }
        if env_var == RESERVED_OUTPUT_ENV {
            // Mirror engine wording from register_build_step (input method).
            let step_name = self.state.name().to_string();
            self.cfg.diagnostics.push(ConfigError::new(format!(
                "build_step(\"{step_name}\").input: env-var name '{RESERVED_OUTPUT_ENV}' is reserved \
                 for the output path injection — use a different name"
            )));
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn build_step_chain_records_inputs_and_env() {
        let mut cfg = Config::new();
        cfg.build_step("disk-image")
            .runner("image-builder")
            .input_crate("KERNEL_PATH", "kernel")
            .input_file("CONFIG_PATH", "disk.toml")
            .input_literal("VERSION", "1.0")
            .env("TARGET_SIZE_MB", "64")
            .output("disk.img");
        let h = cfg.raw.model.build_steps.lookup("disk-image").unwrap();
        let bs = cfg.raw.model.build_steps.get(h).unwrap();
        assert_eq!(bs.runner, "image-builder");
        assert_eq!(bs.output, "disk.img");
        assert_eq!(bs.env.get("TARGET_SIZE_MB").map(String::as_str), Some("64"));
        let keys: Vec<&str> = bs.inputs.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["CONFIG_PATH", "KERNEL_PATH", "VERSION"]);
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_build_step_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.build_step("dup").runner("a").output("o.bin");
        cfg.build_step("dup").runner("b").output("p.bin");
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("dup"));
        assert!(cfg.diagnostics[0].message.contains("defined more than once"));
        let h = cfg.raw.model.build_steps.lookup("dup").unwrap();
        assert_eq!(cfg.raw.model.build_steps.get(h).unwrap().runner, "a");
    }

    #[test]
    fn build_step_reserved_env_var_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.build_step("bs")
            .runner("r")
            .input_crate("GLUON_OUTPUT_PATH", "k")
            .env("GLUON_OUTPUT_PATH", "/whatever")
            .output("o.bin");
        let reserved_count = cfg
            .diagnostics
            .iter()
            .filter(|d| d.message.contains("GLUON_OUTPUT_PATH"))
            .count();
        assert_eq!(reserved_count, 2, "input + env both reject reserved name");
    }

    #[test]
    fn build_step_output_with_path_separator_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.build_step("bs").runner("r").output("sub/dir/o.bin");
        assert!(cfg.diagnostics.iter().any(|d| d.message.contains("path separators")));
    }
}
