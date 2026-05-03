//! Inline builder used by [`CrateBuilder::dep_with`] to populate a
//! single dependency's options (features / version / optional /
//! default_features).
//!
//! `DepBuilder` does NOT carry duplicate / state-machine logic — it
//! holds a `&mut DepDef` directly. Duplicate detection happens in
//! the parent [`CrateBuilder`], which only invokes the closure on
//! the dep it just inserted.

use gluon_model::RawDepDef;

/// Mutator handed to the closure passed to
/// [`crate::builders::CrateBuilder::dep_with`].
///
/// Methods return `&mut Self` for chaining — `DepBuilder` is borrowed
/// from the parent `CrateBuilder`'s arena slot, so the lifetime
/// stays simple.
pub struct DepBuilder<'a> {
    dep: &'a mut RawDepDef,
}

impl<'a> DepBuilder<'a> {
    pub(crate) fn new(dep: &'a mut RawDepDef) -> Self {
        Self { dep }
    }

    /// Replace the feature list with the given values.
    pub fn features(&mut self, features: &[&str]) -> &mut Self {
        self.dep.features = features.iter().map(|s| (*s).to_string()).collect();
        self
    }

    /// Set the crates.io version constraint (e.g. `"2.11"`).
    pub fn version(&mut self, version: &str) -> &mut Self {
        self.dep.version = Some(version.to_string());
        self
    }

    /// Mark this dependency as optional (only linked when a feature
    /// gate enables it).
    pub fn optional(&mut self, optional: bool) -> &mut Self {
        self.dep.optional = optional;
        self
    }

    /// Suppress the dep's default features. Defaults to `true`
    /// (Cargo convention) so this method only needs to be called
    /// when the user wants to disable them.
    pub fn no_default_features(&mut self) -> &mut Self {
        self.dep.default_features = false;
        self
    }
}
