//! Chainable builder for `ProfileDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use gluon_model::{Handle, ProfileDef, TargetDef};

/// Chainable builder returned by [`Config::profile`].
pub struct ProfileBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<()>,
}

impl<'cfg> ProfileBuilder<'cfg> {
    /// Apply `f` to the `ProfileDef` if this builder is not in the
    /// `Duplicate` state. Short-circuits silently on duplicates — the
    /// first definition is preserved.
    fn modify<F: FnOnce(&mut ProfileDef)>(&mut self, f: F) {
        if self.state.is_duplicate() {
            return;
        }
        let name = self.state.name().to_string();
        if let Some(h) = self.cfg.raw.model.profiles.lookup(&name) {
            if let Some(p) = self.cfg.raw.model.profiles.get_mut(h) {
                f(p);
            }
        }
    }

    /// Set the target triple for this profile. The handle is resolved to
    /// a name string immediately; `target_handle` is populated later by
    /// the intern pass.
    pub fn target(self, target: Handle<TargetDef>) -> Self {
        let target_name = self
            .cfg
            .raw
            .model
            .targets
            .get(target)
            .map(|t| t.name.clone());
        let mut me = self;
        me.modify(|p| {
            p.target = target_name.clone();
            // target_handle stays None until the intern pass runs.
        });
        me
    }

    /// Set the optimisation level (`-C opt-level=<level>`).
    pub fn opt_level(mut self, level: u8) -> Self {
        self.modify(|p| p.opt_level = Some(level));
        self
    }

    /// Enable or disable debug info emission.
    pub fn debug_info(mut self, enable: bool) -> Self {
        self.modify(|p| p.debug_info = Some(enable));
        self
    }

    /// Set the LTO kind (e.g. `"thin"`, `"fat"`, `"off"`).
    pub fn lto(mut self, kind: &str) -> Self {
        self.modify(|p| p.lto = Some(kind.to_string()));
        self
    }

    /// Set the name of the binary that is booted by this profile.
    pub fn boot_binary(mut self, name: &str) -> Self {
        self.modify(|p| p.boot_binary = Some(name.to_string()));
        self
    }

    /// Record the name of the parent profile to inherit from. The handle
    /// (`inherits_handle`) is populated by the intern pass, not by the
    /// builder — so only the string is written here.
    pub fn inherits(mut self, parent: &str) -> Self {
        self.modify(|p| p.inherits = Some(parent.to_string()));
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn profile_chains_into_def() {
        let mut cfg = Config::new();
        let t = cfg.target("x86_64-unknown-none");
        cfg.profile("dev")
            .target(t)
            .opt_level(0)
            .debug_info(true)
            .boot_binary("kernel");

        let h = cfg.raw.model.profiles.lookup("dev").expect("dev profile");
        let p = cfg.raw.model.profiles.get(h).unwrap();
        assert_eq!(p.target.as_deref(), Some("x86_64-unknown-none"));
        assert_eq!(p.opt_level, Some(0));
        assert_eq!(p.debug_info, Some(true));
        assert_eq!(p.boot_binary.as_deref(), Some("kernel"));
        assert!(cfg.diagnostics.is_empty(), "no duplicates");
    }

    #[test]
    fn duplicate_profile_does_not_mutate_first_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.profile("dev").opt_level(0);
        cfg.profile("dev").opt_level(3);

        let h = cfg.raw.model.profiles.lookup("dev").unwrap();
        let p = cfg.raw.model.profiles.get(h).unwrap();
        assert_eq!(
            p.opt_level,
            Some(0),
            "first definition's opt_level must be preserved"
        );
        assert_eq!(cfg.raw.model.profiles.len(), 1);
        assert_eq!(
            cfg.diagnostics.len(),
            1,
            "duplicate must push exactly one diagnostic"
        );
        assert!(cfg.diagnostics[0].message.contains("dev"));
        assert!(cfg.diagnostics[0].message.contains("defined more than once"));
    }

    #[test]
    fn inherits_chain_records_string_only() {
        // intern_handle is populated by the intern pass; the builder
        // only writes the string. Stage 4 will replace the parallel field.
        let mut cfg = Config::new();
        cfg.profile("base").opt_level(0);
        cfg.profile("debug").inherits("base").opt_level(0);
        let h = cfg.raw.model.profiles.lookup("debug").unwrap();
        let p = cfg.raw.model.profiles.get(h).unwrap();
        assert_eq!(p.inherits.as_deref(), Some("base"));
        assert!(
            p.inherits_handle.is_none(),
            "intern populates handle, not the builder"
        );
    }
}
