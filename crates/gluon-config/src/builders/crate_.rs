//! Chainable builder for `CrateDef` entries, returned by [`GroupBuilder::add`].

use crate::builders::BuilderState;
use crate::config::Config;
use gluon_model::{CrateDef, CrateType, DepDef, Handle};

/// Chainable builder returned by [`crate::builders::GroupBuilder::add`].
pub struct CrateBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<Handle<CrateDef>>,
}

impl<'cfg> CrateBuilder<'cfg> {
    /// Apply `f` to the `CrateDef` if this builder is `Valid`.
    /// Uses the captured handle directly — no per-call name lookup or string
    /// allocation.
    fn modify<F: FnOnce(&mut CrateDef)>(&mut self, f: F) {
        if let BuilderState::Valid { state: handle, .. } = &self.state {
            if let Some(c) = self.cfg.raw.model.crates.get_mut(*handle) {
                f(c);
            }
        }
    }

    /// Set the crate type (binary, library, proc-macro, etc.).
    pub fn crate_type(mut self, kind: CrateType) -> Self {
        self.modify(|c| c.crate_type = kind);
        self
    }

    /// Override the crate root source file (defaults to the crate type
    /// convention, e.g. `src/lib.rs` or `src/main.rs`).
    pub fn root(mut self, path: &str) -> Self {
        let p = path.to_string();
        self.modify(|c| c.root = Some(p));
        self
    }

    /// Attach a per-crate linker script.
    pub fn linker_script(mut self, path: &str) -> Self {
        let p = path.to_string();
        self.modify(|c| c.linker_script = Some(p));
        self
    }

    /// Append a `--cfg` flag to the crate's compilation flags.
    pub fn cfg_flag(mut self, flag: &str) -> Self {
        let f = flag.to_string();
        self.modify(|c| c.cfg_flags.push(f));
        self
    }

    /// Append a raw `rustc` flag to the crate's compilation flags.
    pub fn rustc_flag(mut self, flag: &str) -> Self {
        let f = flag.to_string();
        self.modify(|c| c.rustc_flags.push(f));
        self
    }

    /// Add a dependency edge. `extern_name` is the key used in `--extern`
    /// flags; `crate_name` is the crate that provides the rlib.
    pub fn dep(mut self, extern_name: &str, crate_name: &str) -> Self {
        let dep = DepDef { crate_name: crate_name.into(), ..Default::default() };
        let key = extern_name.to_string();
        self.modify(|c| {
            c.deps.insert(key, dep);
        });
        self
    }

    /// Add a dependency edge with optional feature/version overrides.
    ///
    /// The closure receives a [`crate::builders::DepBuilder`] and is
    /// invoked synchronously on the just-inserted [`DepDef`]. For
    /// dependencies with no extra options, prefer the simpler
    /// [`Self::dep`] form.
    pub fn dep_with<F: FnOnce(&mut crate::builders::DepBuilder<'_>)>(
        mut self,
        extern_name: &str,
        crate_name: &str,
        configure: F,
    ) -> Self {
        let key = extern_name.to_string();
        let cn = crate_name.to_string();
        self.modify(|c| {
            let dep = c.deps.entry(key).or_insert_with(|| DepDef {
                crate_name: cn,
                ..Default::default()
            });
            configure(&mut crate::builders::DepBuilder::new(dep));
        });
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn cfg_flags_accumulate_in_order() {
        let mut cfg = Config::new();
        let t = cfg.target("x");
        cfg.group("k")
            .target(t)
            .add("foo", "crates/foo")
            .cfg_flag("feature_a")
            .cfg_flag("feature_b")
            .rustc_flag("-C")
            .rustc_flag("opt-level=2");

        let h = cfg.raw.model.crates.lookup("foo").unwrap();
        let k = cfg.raw.model.crates.get(h).unwrap();
        assert_eq!(
            k.cfg_flags,
            vec!["feature_a".to_string(), "feature_b".to_string()]
        );
        assert_eq!(
            k.rustc_flags,
            vec!["-C".to_string(), "opt-level=2".to_string()]
        );
        assert!(cfg.diagnostics.is_empty(), "happy path produces no diagnostics");
    }

    #[test]
    fn dep_with_records_features_and_version() {
        let mut cfg = Config::new();
        let t = cfg.target("x");
        cfg.group("k").target(t).add("foo", "crates/foo").dep_with(
            "mylib",
            "mylib",
            |b| {
                b.features(&["logging", "fancy"]).version("1.2");
            },
        );
        let h = cfg.raw.model.crates.lookup("foo").unwrap();
        let k = cfg.raw.model.crates.get(h).unwrap();
        let dep = k.deps.get("mylib").unwrap();
        assert_eq!(dep.crate_name, "mylib");
        assert_eq!(dep.features, vec!["logging".to_string(), "fancy".to_string()]);
        assert_eq!(dep.version.as_deref(), Some("1.2"));
        assert!(cfg.diagnostics.is_empty());
    }

    #[test]
    fn dep_with_supports_optional_and_no_default_features() {
        let mut cfg = Config::new();
        let t = cfg.target("x");
        cfg.group("k").target(t).add("foo", "crates/foo").dep_with(
            "bitflags",
            "bitflags",
            |b| {
                b.optional(true).no_default_features();
            },
        );
        let h = cfg.raw.model.crates.lookup("foo").unwrap();
        let k = cfg.raw.model.crates.get(h).unwrap();
        let dep = k.deps.get("bitflags").unwrap();
        assert!(dep.optional);
        assert!(!dep.default_features);
    }
}
