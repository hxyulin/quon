//! Chainable builder for `GroupDef` entries.

use crate::builders::crate_::CrateBuilder;
use crate::builders::BuilderState;
use crate::config::Config;
use crate::error::ConfigError;
use gluon_model::{CrateDef, CrateType, GroupDef, Handle, TargetDef};

/// Chainable builder returned by [`Config::group`].
pub struct GroupBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<GroupDef>>,
}

impl<'cfg> GroupBuilder<'cfg> {
    /// Apply `f` to the `GroupDef` if this builder is `Valid`.
    /// Uses the captured handle directly — no per-call name lookup or string
    /// allocation.
    fn modify<F: FnOnce(&mut GroupDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(g) = self.cfg.raw.model.groups.get_mut(*handle) {
                f(g);
            }
        }
    }

    /// Set the target triple for this group. The handle is resolved to a name
    /// string immediately; `target_handle` is populated later by the intern
    /// pass.
    ///
    /// A diagnostic is pushed if `target` is a stale handle that doesn't
    /// index into this `Config`'s target arena (e.g. a handle minted by a
    /// different `Config` instance). The group's `target` field is left as
    /// its default in that case.
    pub fn target(mut self, target: Handle<TargetDef>) -> Self {
        let target_name = self.cfg.raw.model.targets.get(target).map(|t| t.name.clone());
        if target_name.is_none() && !self.state.is_duplicate() {
            // The handle isn't in this Config's target arena — likely a stale
            // handle from a previous Config or a different sibling. Surface it
            // as a diagnostic; the field stays at its default and intern will
            // produce a clearer "group X has no target" error.
            self.cfg.diagnostics.push(ConfigError::new(format!(
                "group \"{}\" received a target handle that doesn't exist in this Config",
                self.state.name(),
            )));
        }
        if let Some(name) = target_name {
            self.modify(|g| g.target = name);
        }
        self
    }

    /// Set the target to the host triple sentinel. The intern pass resolves
    /// `"host"` to the actual host triple at build time.
    pub fn target_host(mut self) -> Self {
        self.modify(|g| g.target = "host".into());
        self
    }

    /// Set the default Rust edition for all crates in this group.
    pub fn edition(mut self, edition: &str) -> Self {
        let e = edition.to_string();
        self.modify(|g| g.default_edition = e);
        self
    }

    /// Set whether crates in this group are linked with the config crate.
    pub fn config(mut self, enabled: bool) -> Self {
        self.modify(|g| g.config = enabled);
        self
    }

    /// Opt out of the custom sysroot and config-crate wiring for all crates
    /// in this group. Required for targets that cannot satisfy a
    /// `core`/`alloc`/`compiler_builtins` build.
    pub fn no_sysroot(mut self) -> Self {
        self.modify(|g| g.no_sysroot = true);
        self
    }

    /// Insert a crate into this group and return a [`CrateBuilder`] to
    /// configure it. Transfers ownership of `cfg` to the new builder — to
    /// add multiple crates, call `cfg.group("name").add(...)` again for each.
    ///
    /// The new `CrateDef` inherits the group's `target` and `default_edition`
    /// at the time of this call.
    pub fn add(self, crate_name: &str, path: &str) -> CrateBuilder<'cfg> {
        let cfg = self.cfg;

        // If the parent group was a duplicate, propagate the flag so chained
        // CrateBuilder methods also no-op. We do not insert the crate either,
        // since the group is not the one the author intended.
        if self.state.is_duplicate() {
            let state = BuilderState::Duplicate { name: crate_name.into(), span: None };
            return CrateBuilder { cfg, state };
        }

        // Resolve group-level defaults for the new CrateDef.
        let (edition, target, group_name) = {
            if let BuilderState::Valid { state: handle, .. } = &self.state {
                if let Some(g) = cfg.raw.model.groups.get(*handle) {
                    (g.default_edition.clone(), g.target.clone(), g.name.clone())
                } else {
                    ("2024".to_string(), "host".to_string(), self.state.name().to_string())
                }
            } else {
                unreachable!("is_duplicate() already handled above")
            }
        };

        let new_crate = CrateDef {
            name: crate_name.into(),
            path: path.into(),
            edition,
            crate_type: CrateType::Lib,
            target,
            target_handle: None,
            deps: Default::default(),
            dev_deps: Default::default(),
            features: Vec::new(),
            root: None,
            linker_script: None,
            group: group_name.clone(),
            group_handle: None,
            is_project_crate: true,
            cfg_flags: Vec::new(),
            rustc_flags: Vec::new(),
            requires_config: Vec::new(),
            artifact_deps: Vec::new(),
            artifact_env: Default::default(),
            span: None,
        };

        let (crate_handle, inserted) = cfg.raw.model.crates.insert(crate_name.into(), new_crate);
        if !inserted {
            cfg.diagnostics.push(ConfigError::new(format!(
                "crate \"{crate_name}\" is defined more than once"
            )));
            let state = BuilderState::Duplicate { name: crate_name.into(), span: None };
            return CrateBuilder { cfg, state };
        }

        // Record the crate under its group for later resolution.
        if let BuilderState::Valid { state: group_handle, .. } = &self.state {
            if let Some(g) = cfg.raw.model.groups.get_mut(*group_handle) {
                g.crates.push(crate_name.to_string());
            }
        }

        let state =
            BuilderState::Valid { state: crate_handle, name: crate_name.into(), span: None };
        CrateBuilder { cfg, state }
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;
    use gluon_model::CrateType;

    #[test]
    fn group_with_crate_full_chain() {
        let mut cfg = Config::new();
        let t = cfg.target("x");
        cfg.group("kernel")
            .target(t)
            .edition("2021")
            .add("kernel", "crates/kernel")
            .crate_type(CrateType::Bin)
            .root("src/main.rs")
            .linker_script("crates/kernel/kernel.ld")
            .dep("foo", "foo");

        let kh = cfg.raw.model.crates.lookup("kernel").unwrap();
        let k = cfg.raw.model.crates.get(kh).unwrap();
        assert_eq!(k.path, "crates/kernel");
        assert_eq!(k.crate_type, CrateType::Bin);
        assert_eq!(k.root.as_deref(), Some("src/main.rs"));
        assert_eq!(k.linker_script.as_deref(), Some("crates/kernel/kernel.ld"));
        assert_eq!(k.target, "x");
        assert_eq!(k.group, "kernel");
        let dep = k.deps.get("foo").unwrap();
        assert_eq!(dep.crate_name, "foo");
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn target_host_sets_sentinel() {
        let mut cfg = Config::new();
        cfg.group("host_tools").target_host().edition("2021");
        let h = cfg.raw.model.groups.lookup("host_tools").unwrap();
        let g = cfg.raw.model.groups.get(h).unwrap();
        assert_eq!(g.target, "host");
    }

    #[test]
    fn duplicate_group_short_circuits_and_pushes_diagnostic() {
        let mut cfg = Config::new();
        cfg.group("k").edition("2021");
        cfg.group("k").edition("2024");
        let g = cfg.raw.model.groups.lookup("k").unwrap();
        assert_eq!(cfg.raw.model.groups.get(g).unwrap().default_edition, "2021");
        assert_eq!(cfg.diagnostics.len(), 1);
        assert!(cfg.diagnostics[0].message.contains("k"));
        assert!(cfg.diagnostics[0].message.contains("defined more than once"));
    }

    #[test]
    fn duplicate_crate_via_add_short_circuits() {
        let mut cfg = Config::new();
        let t = cfg.target("x");
        cfg.group("a").target(t).add("foo", "crates/foo").crate_type(CrateType::Bin);
        cfg.group("b").target(t).add("foo", "crates/other").crate_type(CrateType::Lib);
        let h = cfg.raw.model.crates.lookup("foo").unwrap();
        let k = cfg.raw.model.crates.get(h).unwrap();
        assert_eq!(k.path, "crates/foo");
        assert_eq!(k.crate_type, CrateType::Bin);
        assert_eq!(
            cfg.diagnostics.len(),
            1,
            "duplicate crate must push exactly one diagnostic"
        );
        assert!(cfg.diagnostics[0].message.contains("foo"));
    }

    #[test]
    fn target_with_stale_handle_pushes_diagnostic() {
        // Stage 1 limitation: Handle<TargetDef> is just a u32 index. To
        // exercise the stale-handle guard the receiving Config's target arena
        // must be empty (or smaller than the stale index). See ProfileBuilder
        // for the same pattern.
        let mut cfg_a = Config::new();
        let stale = cfg_a.target("from-a");

        let mut cfg_b = Config::new();
        cfg_b.group("kernel").target(stale);
        assert_eq!(cfg_b.diagnostics.len(), 1);
        assert!(cfg_b.diagnostics[0].message.contains("doesn't exist in this Config"));
    }
}
