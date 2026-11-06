//! Chainable builder for `ProfileDef` entries.

use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{BootMode, Handle, RawProfileDef, TargetDef};

/// Chainable builder returned by [`Config::profile`].
pub struct ProfileBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<RawProfileDef>>,
}

impl<'cfg> ProfileBuilder<'cfg> {
    /// Apply `f` to the `RawProfileDef` if this builder is `Valid`.
    /// Uses the captured handle directly — no per-call name lookup or string
    /// allocation.
    fn modify<F: FnOnce(&mut RawProfileDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state
            && let Some(p) = self.cfg.raw.profiles.get_mut(*handle)
        {
            f(p);
        }
    }

    /// Set the target triple for this profile. The handle is resolved to a
    /// name string immediately; intern resolves names against arenas.
    ///
    /// A diagnostic is pushed if `target` is a stale handle that doesn't
    /// index into this `Config`'s target arena (e.g. a handle minted by a
    /// different `Config` instance). The profile's `target` field is left
    /// `None` in that case rather than silently recording an incorrect name.
    pub fn target(mut self, target: Handle<TargetDef>) -> Self {
        let target_name = self.cfg.raw.targets.get(target).map(|t| t.name.clone());
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

    /// Build for the host instead of a declared target: a profile for a
    /// host tool. It builds no sysroot and no config crate, and with no
    /// `.runner(..)` named, `gluon run` runs the run binary directly.
    pub fn target_host(mut self) -> Self {
        self.modify(|p| p.target = Some("host".into()));
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

    /// The binary `gluon run` starts with the profile's runner. With a
    /// run binary set, the profile builds only the crates it reaches.
    pub fn run_binary(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|p| p.run_binary = Some(n));
        self
    }

    /// The runner, declared with `cfg.runner(name)`, that executes the
    /// run binary, the integration tests and the boot tests. Inherited
    /// through [`inherits`](Self::inherits).
    pub fn runner(mut self, name: &str) -> Self {
        let n = name.to_string();
        self.modify(|p| p.runner = Some(n));
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

    /// Pin the QEMU boot mode for this profile. Accepts the same string
    /// vocabulary as [`crate::builders::QemuBuilder::boot_mode`]:
    /// `"direct"`, `"uefi"`, or `"bios"`.
    ///
    /// When set, this overrides the project-level `qemu().boot_mode(...)`
    /// for runs of this profile — useful for projects whose `dev-uefi`
    /// and `dev-bios` profiles share a compilation graph but boot
    /// differently.
    pub fn qemu_boot_mode(mut self, mode: &str) -> Self {
        let parsed = match mode {
            "direct" => Some(BootMode::Direct),
            "uefi" => Some(BootMode::Uefi),
            "bios" => Some(BootMode::Bios),
            other => {
                if !self.state.is_duplicate() {
                    self.cfg.diagnostics.push(ConfigError::new(format!(
                        "profile.qemu_boot_mode: expected \"direct\", \"uefi\", or \"bios\", \
                         got \"{other}\""
                    )));
                }
                None
            }
        };
        if let Some(m) = parsed {
            self.modify(|p| p.qemu_boot_mode = Some(m));
        }
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
            .run_binary("kernel");

        let h = cfg.raw.profiles.lookup("dev").expect("dev profile");
        let p = cfg.raw.profiles.get(h).unwrap();
        assert_eq!(p.target.as_deref(), Some("x86_64-unknown-none"));
        assert_eq!(p.opt_level, Some(0));
        assert_eq!(p.debug_info, Some(true));
        assert_eq!(p.run_binary.as_deref(), Some("kernel"));
        assert!(cfg.diagnostics.is_empty(), "no duplicates");
    }

    #[test]
    fn duplicate_profile_does_not_mutate_first_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.profile("dev").opt_level(0);
        cfg.profile("dev").opt_level(3);

        let h = cfg.raw.profiles.lookup("dev").unwrap();
        let p = cfg.raw.profiles.get(h).unwrap();
        assert_eq!(
            p.opt_level,
            Some(0),
            "first definition's opt_level must be preserved"
        );
        assert_eq!(cfg.raw.profiles.len(), 1);
        assert_eq!(
            cfg.diagnostics.len(),
            1,
            "duplicate must push exactly one diagnostic"
        );
        assert!(cfg.diagnostics[0].message.contains("dev"));
        assert!(
            cfg.diagnostics[0]
                .message
                .contains("defined more than once")
        );
    }

    #[test]
    fn qemu_boot_mode_parses_known_values() {
        use gluon_model::BootMode;
        let mut cfg = Config::new();
        cfg.profile("dev-uefi").qemu_boot_mode("uefi");
        cfg.profile("dev-bios").qemu_boot_mode("bios");
        cfg.profile("dev-direct").qemu_boot_mode("direct");

        let uefi = cfg.raw.profiles.lookup("dev-uefi").unwrap();
        let bios = cfg.raw.profiles.lookup("dev-bios").unwrap();
        let direct = cfg.raw.profiles.lookup("dev-direct").unwrap();
        assert_eq!(
            cfg.raw.profiles.get(uefi).unwrap().qemu_boot_mode,
            Some(BootMode::Uefi)
        );
        assert_eq!(
            cfg.raw.profiles.get(bios).unwrap().qemu_boot_mode,
            Some(BootMode::Bios)
        );
        assert_eq!(
            cfg.raw.profiles.get(direct).unwrap().qemu_boot_mode,
            Some(BootMode::Direct)
        );
        assert!(
            cfg.diagnostics.is_empty(),
            "valid values must not push diagnostics"
        );
    }

    #[test]
    fn qemu_boot_mode_rejects_unknown_value() {
        let mut cfg = Config::new();
        cfg.profile("dev").qemu_boot_mode("kexec");

        let h = cfg.raw.profiles.lookup("dev").unwrap();
        let p = cfg.raw.profiles.get(h).unwrap();
        assert!(
            p.qemu_boot_mode.is_none(),
            "unknown value must leave field unset"
        );
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("kexec"));
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

        assert_eq!(
            cfg_b.diagnostics.len(),
            1,
            "stale handle must push exactly one diagnostic"
        );
        assert!(
            cfg_b.diagnostics[0]
                .message
                .contains("doesn't exist in this Config"),
            "got: {}",
            cfg_b.diagnostics[0].message,
        );
        // Profile's target should be unset (not silently set to a wrong name).
        let h = cfg_b.raw.profiles.lookup("dev").unwrap();
        let p = cfg_b.raw.profiles.get(h).unwrap();
        assert!(p.target.is_none());
    }
}
