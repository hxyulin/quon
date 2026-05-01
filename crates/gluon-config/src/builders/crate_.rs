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

    /// Inject an environment variable at rustc-spawn time whose value
    /// is the referenced crate's primary output path (absolute,
    /// canonicalised). Automatically adds the referenced crate to
    /// `artifact_deps` so the DAG enforces build ordering.
    ///
    /// Use case: a bootloader crate declares
    /// `artifact_env("KERNEL_PATH", "kernel")` and its source does
    /// `static KERNEL: &[u8] = include_bytes!(env!("KERNEL_PATH"));` to
    /// embed the kernel binary.
    ///
    /// Mirrors the engine's empty-name validation:
    /// `"crate '{name}' artifact_env: environment variable name must not be empty"`
    /// and `... referenced crate name must not be empty`.
    pub fn artifact_env(mut self, key: &str, dep_name: &str) -> Self {
        if self.state.is_duplicate() {
            return self;
        }
        if key.is_empty() {
            let name = self.state.name().to_string();
            self.cfg.diagnostics.push(crate::error::ConfigError::new(format!(
                "crate '{name}' artifact_env: environment variable name must not be empty"
            )));
            return self;
        }
        if dep_name.is_empty() {
            let name = self.state.name().to_string();
            self.cfg.diagnostics.push(crate::error::ConfigError::new(format!(
                "crate '{name}' artifact_env: referenced crate name must not be empty"
            )));
            return self;
        }
        let key_s = key.to_string();
        let dep_s = dep_name.to_string();
        self.modify(|c| {
            c.artifact_env.insert(key_s, dep_s.clone());
            if !c.artifact_deps.iter().any(|d| d == &dep_s) {
                c.artifact_deps.push(dep_s);
            }
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

    #[test]
    fn artifact_env_inserts_pair_and_auto_dep_edge() {
        let mut cfg = Config::new();
        let t = cfg.target("x");
        cfg.group("kernel_group").target(t).add("kernel", "crates/kernel");
        cfg.group("bootloader_group")
            .target(t)
            .add("bootloader", "crates/bootloader")
            .artifact_env("KERNEL_PATH", "kernel");

        let h = cfg.raw.model.crates.lookup("bootloader").unwrap();
        let k = cfg.raw.model.crates.get(h).unwrap();
        assert_eq!(
            k.artifact_env.get("KERNEL_PATH").map(|s| s.as_str()),
            Some("kernel"),
        );
        assert_eq!(k.artifact_deps, vec!["kernel".to_string()]);
    }

    #[test]
    fn artifact_env_idempotent_on_duplicate_edge() {
        // Calling artifact_env twice for the same dep_name should not
        // duplicate the artifact_deps entry.
        let mut cfg = Config::new();
        let t = cfg.target("x");
        cfg.group("kernel_group").target(t).add("kernel", "crates/kernel");
        cfg.group("bootloader_group")
            .target(t)
            .add("bootloader", "crates/bootloader")
            .artifact_env("KERNEL_A", "kernel")
            .artifact_env("KERNEL_B", "kernel");
        let h = cfg.raw.model.crates.lookup("bootloader").unwrap();
        let k = cfg.raw.model.crates.get(h).unwrap();
        assert_eq!(k.artifact_deps, vec!["kernel".to_string()]);
        assert_eq!(k.artifact_env.len(), 2);
    }
}
