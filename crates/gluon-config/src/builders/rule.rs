//! Chainable builder for `RuleDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use gluon_model::{Handle, RuleDef, RuleHandler};

/// Chainable builder returned by [`Config::rule`].
pub struct RuleBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<RuleDef>>,
}

impl<'cfg> RuleBuilder<'cfg> {
    /// Apply `f` to the `RuleDef` if this builder is `Valid`.
    /// Uses the captured handle directly — no per-call name lookup or string
    /// allocation.
    fn modify<F: FnOnce(&mut RuleDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(r) = self.cfg.raw.model.rules.get_mut(*handle) {
                f(r);
            }
        }
    }

    /// Set the handler to a named builtin. Overwrites any previously set
    /// handler (engine parity: `handler()` is a last-write-wins setter).
    pub fn handler(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|r| r.handler = RuleHandler::Builtin(n));
        self
    }

    /// Set the handler to a user-provided script. Overwrites any previously
    /// set handler.
    pub fn on_execute(mut self, script: &str) -> Self {
        let s = script.to_string();
        self.modify(|r| r.handler = RuleHandler::Script(s));
        self
    }

    /// Set the working directory for rule execution. `None` defaults to the
    /// build directory at runtime.
    pub fn working_dir(mut self, dir: &str) -> Self {
        let d = dir.to_string();
        self.modify(|r| r.working_dir = Some(d));
        self
    }

    /// Append a list of input names to the rule. Engine parity: the engine
    /// uses `append`, so repeated calls accumulate rather than overwrite.
    pub fn inputs(mut self, items: &[&str]) -> Self {
        let v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        self.modify(|r| r.inputs.extend(v));
        self
    }

    /// Append a list of output names to the rule. Engine parity: uses
    /// `append`, repeated calls accumulate.
    pub fn outputs(mut self, items: &[&str]) -> Self {
        let v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        self.modify(|r| r.outputs.extend(v));
        self
    }

    /// Append a single output name. Engine parity: mirrors the `output`
    /// method that the engine exposes alongside `outputs`.
    pub fn output(mut self, single: &str) -> Self {
        let s = single.to_string();
        self.modify(|r| r.outputs.push(s));
        self
    }

    /// Append a list of rule names this rule depends on. Engine parity: uses
    /// `append`, repeated calls accumulate.
    pub fn depends_on(mut self, items: &[&str]) -> Self {
        let v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        self.modify(|r| r.depends_on.extend(v));
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn rule_chain_sets_typed_fields() {
        let mut cfg = Config::new();
        cfg.rule("my_rule")
            .handler("exec")
            .inputs(&["a", "b"])
            .outputs(&["out.bin"])
            .depends_on(&["other_rule"])
            .working_dir("project_root");
        let h = cfg.raw.model.rules.lookup("my_rule").unwrap();
        let r = cfg.raw.model.rules.get(h).unwrap();
        match &r.handler {
            gluon_model::RuleHandler::Builtin(s) if s == "exec" => {}
            other => panic!("expected Builtin(\"exec\"), got {other:?}"),
        }
        assert_eq!(r.inputs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(r.outputs, vec!["out.bin".to_string()]);
        assert_eq!(r.depends_on, vec!["other_rule".to_string()]);
        assert_eq!(r.working_dir.as_deref(), Some("project_root"));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn duplicate_rule_short_circuits_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.rule("dup").handler("exec");
        cfg.rule("dup").handler("script");
        let h = cfg.raw.model.rules.lookup("dup").unwrap();
        let r = cfg.raw.model.rules.get(h).unwrap();
        match &r.handler {
            gluon_model::RuleHandler::Builtin(s) if s == "exec" => {}
            other => panic!("first handler must be preserved, got {other:?}"),
        }
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("dup"));
        assert!(cfg.diagnostics[0].message.contains("defined more than once"));
    }

    #[test]
    fn rule_on_execute_records_script_handler() {
        let mut cfg = Config::new();
        cfg.rule("scripted").on_execute("echo hi");
        let h = cfg.raw.model.rules.lookup("scripted").unwrap();
        let r = cfg.raw.model.rules.get(h).unwrap();
        match &r.handler {
            gluon_model::RuleHandler::Script(s) if s == "echo hi" => {}
            other => panic!("expected Script handler, got {other:?}"),
        }
    }
}
