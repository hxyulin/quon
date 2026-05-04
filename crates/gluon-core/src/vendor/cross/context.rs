//! Side-table the scheduler consults to dispatch vendor-crate builds.
//!
//! Vendor crates are not part of the user-declared [`gluon_model::BuildModel`]
//! arena set — they're synthesised at `vendor_sync` time from cargo's
//! resolution output. To keep `BuildModel` faithful to what the user wrote in
//! their `gluon.rs`, the scheduler reads vendor builds from this separate
//! context object rather than from a model arena.
//!
//! # Identification
//!
//! Each registered build gets a [`VendorBuildId`] (a thin newtype around a
//! `u32` index into the backing `Vec`). The id is what the
//! `DagNode::VendorCrate` variant carries; everything else lives behind it.
//! Lookups from the consumer side go through `(target, cargo_target_name)`
//! to a `VendorBuildId` — that's the same compound key shape as
//! [`gluon_model::ExternalArtifactKey`], so a future "what artifact does
//! my consumer's `--extern foo` resolve to" lookup is a single map hit.
//!
//! # Why an `Err` on duplicate insert
//!
//! Each cross target gets its own `cargo metadata` resolution, so within a
//! target the `(target, cargo_target_name)` key is unique by construction.
//! Across targets, host-pinned crates (proc-macros and their transitive
//! deps) *can* collide if two cross targets each pull in the same proc-macro
//! through different chains. Whether the two host resolutions are identical
//! depends on cargo's feature unification, which is per-resolution. We
//! surface the collision here rather than silently merging because the
//! resolutions could legitimately disagree on features or version, and a
//! quiet last-wins would mask that.
//!
//! Today the orchestration layer can treat `DuplicateKey` as fatal — MVP
//! scope. A later iteration may layer "identical → reuse, different →
//! diagnostic" on top of this primitive.

use std::collections::BTreeMap;

use gluon_model::TargetSpec;

use super::graph::VendorCrateBuild;

/// Stable handle for a registered [`VendorCrateBuild`].
///
/// The wrapped `u32` is the backing `Vec`'s index; ids are assigned in
/// insertion order and never reused. Newtyped (rather than a raw `u32`)
/// so it's harder to accidentally pass an unrelated index where a vendor
/// id is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VendorBuildId(u32);

impl VendorBuildId {
    pub fn index(self) -> u32 {
        self.0
    }

    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Display for VendorBuildId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "vendor#{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error(
        "duplicate vendor build registration for ({target:?}, {cargo_target_name}); \
         existing id is {existing_id}"
    )]
    DuplicateKey {
        target: TargetSpec,
        cargo_target_name: String,
        existing_id: VendorBuildId,
    },
}

/// All `VendorCrateBuild`s the scheduler should dispatch, keyed for
/// `(target, cargo_target_name)` lookups.
///
/// Iteration order is insertion order, which is deterministic given that
/// `vendor_sync` walks cross targets in `BTreeMap` order and within a
/// target receives builds in `BTreeMap`-sorted order from
/// [`super::graph::build_vendor_nodes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VendorBuildContext {
    builds: Vec<VendorCrateBuild>,
    by_key: BTreeMap<(TargetSpec, String), VendorBuildId>,
}

impl VendorBuildContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.builds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.builds.is_empty()
    }

    /// Register a new build. Returns the assigned id, or
    /// [`ContextError::DuplicateKey`] if the same
    /// `(target, cargo_target_name)` was already registered (in which
    /// case the existing id is preserved untouched).
    pub fn add(&mut self, build: VendorCrateBuild) -> Result<VendorBuildId, ContextError> {
        let key = (build.target, build.cargo_target_name.clone());
        if let Some(&existing) = self.by_key.get(&key) {
            return Err(ContextError::DuplicateKey {
                target: key.0,
                cargo_target_name: key.1,
                existing_id: existing,
            });
        }
        let id = VendorBuildId(self.builds.len() as u32);
        self.builds.push(build);
        self.by_key.insert(key, id);
        Ok(id)
    }

    pub fn lookup(&self, target: TargetSpec, cargo_target_name: &str) -> Option<VendorBuildId> {
        // Borrowed-key lookup would need `Borrow<(TargetSpec, str)>`,
        // which doesn't exist for tuples. Keep the allocation here —
        // this lookup is once-per-edge during DAG construction, not a
        // hot path.
        self.by_key
            .get(&(target, cargo_target_name.to_string()))
            .copied()
    }

    pub fn get(&self, id: VendorBuildId) -> Option<&VendorCrateBuild> {
        self.builds.get(id.as_usize())
    }

    pub fn iter(&self) -> impl Iterator<Item = (VendorBuildId, &VendorCrateBuild)> {
        self.builds
            .iter()
            .enumerate()
            .map(|(i, b)| (VendorBuildId(i as u32), b))
    }
}

// -------------------- tests --------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::cross::graph::VendorCrateBuild;
    use crate::vendor::cross::metadata::{CrateId, CrateKind};
    use gluon_model::{Handle, TargetDef};
    use std::path::PathBuf;

    fn target_handle(idx: u32) -> Handle<TargetDef> {
        Handle::<TargetDef>::new(idx)
    }

    fn build(name: &str, ver: &str, target: TargetSpec, kind: CrateKind) -> VendorCrateBuild {
        VendorCrateBuild {
            crate_id: CrateId { name: name.into(), version: ver.into() },
            cargo_target_name: name.replace('-', "_"),
            target,
            kind,
            manifest_dir: PathBuf::from(format!("/vendor/{name}-{ver}")),
            lib_path: PathBuf::from(format!("/vendor/{name}-{ver}/src/lib.rs")),
            edition: "2021".into(),
            features: vec![],
            deps: vec![],
        }
    }

    #[test]
    fn empty_context_has_no_entries() {
        let ctx = VendorBuildContext::new();
        assert!(ctx.is_empty());
        assert_eq!(ctx.len(), 0);
        assert_eq!(ctx.iter().count(), 0);
        assert!(ctx.lookup(TargetSpec::Host, "anything").is_none());
    }

    #[test]
    fn add_assigns_sequential_ids_and_supports_lookup() {
        let mut ctx = VendorBuildContext::new();
        let id_a = ctx
            .add(build("a", "1.0.0", TargetSpec::Cross(target_handle(0)), CrateKind::Lib))
            .expect("first add ok");
        let id_b = ctx
            .add(build("b", "1.0.0", TargetSpec::Cross(target_handle(0)), CrateKind::Lib))
            .expect("second add ok");
        assert_eq!(id_a.index(), 0);
        assert_eq!(id_b.index(), 1);
        assert_eq!(ctx.len(), 2);

        // Lookups round-trip via cargo_target_name (post-hyphen-norm).
        assert_eq!(
            ctx.lookup(TargetSpec::Cross(target_handle(0)), "a"),
            Some(id_a),
        );
        assert_eq!(
            ctx.lookup(TargetSpec::Cross(target_handle(0)), "b"),
            Some(id_b),
        );

        // Wrong target → miss, even with a known name.
        assert!(ctx.lookup(TargetSpec::Host, "a").is_none());
        assert!(
            ctx.lookup(TargetSpec::Cross(target_handle(99)), "a")
                .is_none()
        );
    }

    #[test]
    fn lookup_uses_cargo_target_name_not_raw_name() {
        // proc-macro2 (raw) → proc_macro2 (cargo target) — lookups use
        // the underscored form to match `--extern` and `ExternalArtifactKey`.
        let mut ctx = VendorBuildContext::new();
        let id = ctx
            .add(build(
                "proc-macro2",
                "1.0.0",
                TargetSpec::Host,
                CrateKind::ProcMacro,
            ))
            .expect("add ok");
        assert_eq!(ctx.lookup(TargetSpec::Host, "proc_macro2"), Some(id));
        assert!(
            ctx.lookup(TargetSpec::Host, "proc-macro2").is_none(),
            "raw name must not match the cargo-normalised key",
        );
    }

    #[test]
    fn same_name_under_different_targets_coexists() {
        // Same `cargo_target_name` is fine as long as the target differs.
        // Realistic case: bitflags built once for the kernel target and
        // once for host (because a proc-macro pulls it in transitively).
        let mut ctx = VendorBuildContext::new();
        let cross = ctx
            .add(build(
                "bitflags",
                "2.11.0",
                TargetSpec::Cross(target_handle(0)),
                CrateKind::Lib,
            ))
            .expect("cross ok");
        let host = ctx
            .add(build("bitflags", "2.11.0", TargetSpec::Host, CrateKind::Lib))
            .expect("host ok");
        assert_ne!(cross, host);
        assert_eq!(
            ctx.lookup(TargetSpec::Cross(target_handle(0)), "bitflags"),
            Some(cross),
        );
        assert_eq!(ctx.lookup(TargetSpec::Host, "bitflags"), Some(host));
    }

    #[test]
    fn duplicate_target_and_name_returns_error_without_mutating() {
        let mut ctx = VendorBuildContext::new();
        let first = ctx
            .add(build("a", "1.0.0", TargetSpec::Host, CrateKind::Lib))
            .expect("first ok");
        let err = ctx
            .add(build("a", "1.0.0", TargetSpec::Host, CrateKind::Lib))
            .expect_err("duplicate must error");
        match err {
            ContextError::DuplicateKey {
                target,
                cargo_target_name,
                existing_id,
            } => {
                assert_eq!(target, TargetSpec::Host);
                assert_eq!(cargo_target_name, "a");
                assert_eq!(existing_id, first);
            }
        }
        // No new entry was added; the original id still resolves.
        assert_eq!(ctx.len(), 1);
        assert_eq!(ctx.lookup(TargetSpec::Host, "a"), Some(first));
    }

    #[test]
    fn iter_yields_insertion_order() {
        let mut ctx = VendorBuildContext::new();
        let names = ["z", "a", "m"];
        let ids: Vec<VendorBuildId> = names
            .iter()
            .map(|n| {
                ctx.add(build(n, "1.0.0", TargetSpec::Host, CrateKind::Lib))
                    .expect("add ok")
            })
            .collect();

        let observed: Vec<(VendorBuildId, &str)> = ctx
            .iter()
            .map(|(id, b)| (id, b.crate_id.name.as_str()))
            .collect();
        assert_eq!(
            observed,
            vec![(ids[0], "z"), (ids[1], "a"), (ids[2], "m")],
            "iter must follow insertion order, not key order",
        );
    }

    #[test]
    fn get_returns_registered_build() {
        let mut ctx = VendorBuildContext::new();
        let id = ctx
            .add(build("a", "1.0.0", TargetSpec::Host, CrateKind::Lib))
            .expect("ok");
        let got = ctx.get(id).expect("present");
        assert_eq!(got.crate_id.name, "a");

        // Out-of-range id → None (defensive — shouldn't happen because
        // ids only come from this context, but the API should still
        // tell the truth rather than panic).
        let bogus = VendorBuildId(999);
        assert!(ctx.get(bogus).is_none());
    }
}
