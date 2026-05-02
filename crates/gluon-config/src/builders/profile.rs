//! Chainable builder for `ProfileDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{Handle, ProfileDef, TargetDef};

/// Chainable builder returned by [`Config::profile`].
pub struct ProfileBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<ProfileDef>>,
}

impl<'cfg> ProfileBuilder<'cfg> {
    /// Apply `f` to the `ProfileDef` if this builder is `Valid`.
    /// Uses the captured handle directly — no per-call name lookup or string
    /// allocation.
    fn modify<F: FnOnce(&mut ProfileDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(p) = self.cfg.raw.model.profiles.get_mut(*handle) {
                f(p);
            }
        }
    }

    /// Set the target triple for this profile. The handle is resolved to a
    /// name string immediately; `target_handle` is populated later by the
    /// intern pass.
    ///
    /// A diagnostic is pushed if `target` is a stale handle that doesn't
    /// index into this `Config`'s target arena (e.g. a handle minted by a
    /// different `Config` instance). The profile's `target` field is left
    /// `None` in that case rather than silently recording an incorrect name.
    pub fn target(mut self, target: Handle<TargetDef>) -> Self {
        let target_name = self.cfg.raw.model.targets.get(target).map(|t| t.name.clone());
        if target_name.is_none() && !self.state.is_duplicate() {
            // The handle isn't in this Config's target arena — likely a stale
            // handle from a previous Config or a different sibling. Surface it
            // as a diagnostic; the field stays None and intern will produce
            // a clearer "profile X has no target" error.
            self.cfg.diagnostics.push(ConfigError::new(format!(
                "profile \"{}\" received a target handle that doesn't exist in this Config",
                self.state.name(),
            )));
        }
        self.modify(|p| p.target = target_name);
        self
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
        let k = kind.to_string();
        self.modify(|p| p.lto = Some(k));
        self
    }

    /// Set the name of the binary that is booted by this profile.
    pub fn boot_binary(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|p| p.boot_binary = Some(n));
        self
    }

    /// Record the name of the parent profile to inherit from. The handle
    /// (`inherits_handle`) is populated by the intern pass, not by the
    /// builder — so only the string is written here.
    pub fn inherits(mut self, parent: &str) -> Self {
        let p_name = parent.to_string();
        self.modify(|pf| pf.inherits = Some(p_name));
        self
    }

    /// Set the named preset to apply to this profile's config options.
    /// The preset is looked up by name during the resolver pass; only
    /// the string is written here (mirrors how `inherits` works).
    pub fn preset(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|p| p.preset = Some(n));
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

    #[test]
    fn target_with_stale_handle_pushes_diagnostic() {
        // `Handle<T>` is a plain u32 arena index — there is no arena identity
        // baked in. A handle from cfg_a is only "stale" relative to cfg_b when
        // its index is out-of-bounds for cfg_b's arena. We set cfg_b up with an
        // empty target arena so index 0 from cfg_a falls outside it.
        //
        // The case where cfg_b happens to have a target at the same index is
        // indistinguishable from a valid handle at the type level; that requires
        // arena-identity in Handle<T>, which is a separate concern.
        let mut cfg_a = Config::new();
        let stale = cfg_a.target("from-a"); // index 0 in cfg_a

        let mut cfg_b = Config::new(); // target arena is empty — index 0 is OOB
        cfg_b.profile("dev").target(stale);

        assert_eq!(cfg_b.diagnostics.len(), 1, "stale handle must push exactly one diagnostic");
        assert!(
            cfg_b.diagnostics[0].message.contains("doesn't exist in this Config"),
            "got: {}", cfg_b.diagnostics[0].message,
        );
        // Profile's target should be unset (not silently set to a wrong name).
        let h = cfg_b.raw.model.profiles.lookup("dev").unwrap();
        let p = cfg_b.raw.model.profiles.get(h).unwrap();
        assert!(p.target.is_none());
    }
}
