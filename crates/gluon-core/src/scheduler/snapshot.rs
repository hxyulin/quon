//! `ArtifactsSnapshot` — owned, immutable view of the scheduler's
//! per-node output maps, plus the `NodeOutput` enum that per-variant
//! dispatch helpers return for merging. See
//! `docs/src/architecture/concurrency.md` for the dispatch protocol
//! (clone snapshot → run helper lock-free → merge `NodeOutput`).

use crate::compile::ArtifactMap;
use crate::vendor::cross::context::VendorBuildId;
use gluon_model::{BuildStepDef, CrateDef, EspDef, Handle, ImageDef, TargetDef};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Owned bundle of every output map the scheduler maintains across a
/// build. Cloned per dispatch so per-node helpers operate against a
/// stable point-in-time view, avoiding both lock-across-helper and
/// the four-mutex pattern that preceded Stage 8.
#[derive(Debug, Default, Clone)]
pub(crate) struct ArtifactsSnapshot {
    pub(crate) sysroots: BTreeMap<Handle<TargetDef>, PathBuf>,
    pub(crate) artifacts: ArtifactMap,
    pub(crate) esps: BTreeMap<Handle<EspDef>, PathBuf>,
    pub(crate) images: BTreeMap<Handle<ImageDef>, PathBuf>,
    /// Per-vendor-build output paths (rlib for libs, .so/.dylib for
    /// proc-macros). Keyed by [`VendorBuildId`] so the consumer-side
    /// dispatch can look up its dep's artifact in O(log n) without
    /// reconstructing the (target, name) compound key. Populated by
    /// [`NodeOutput::VendorCrate`] merges; read by the consumer's
    /// dispatch arm when assembling its `--extern` map and by the
    /// downstream `auto_register_vendored_deps` step that promotes
    /// these paths into [`gluon_model::BuildModel::external_artifacts`].
    pub(crate) vendor_artifacts: BTreeMap<VendorBuildId, PathBuf>,
}

/// What a per-variant `dispatch_*` helper produces. The scheduler's
/// dispatch arm folds this into the canonical `Mutex<ArtifactsSnapshot>`
/// after the helper returns. Each variant carries `was_cached` so the
/// `built`/`cached` counters can be incremented at merge time.
#[derive(Debug)]
pub(crate) enum NodeOutput {
    Sysroot {
        target: Handle<TargetDef>,
        path: PathBuf,
        was_cached: bool,
    },
    ConfigCrate {
        target: Handle<TargetDef>,
        rlib_path: PathBuf,
        was_cached: bool,
    },
    Crate {
        handle: Handle<CrateDef>,
        out_path: PathBuf,
        was_cached: bool,
    },
    Esp {
        handle: Handle<EspDef>,
        path: PathBuf,
        was_cached: bool,
    },
    Image {
        handle: Handle<ImageDef>,
        path: PathBuf,
        was_cached: bool,
    },
    BuildStep {
        handle: Handle<BuildStepDef>,
        out_path: PathBuf,
        was_cached: bool,
    },
    /// One vendored-crate build's output. Lands in
    /// [`ArtifactsSnapshot::vendor_artifacts`] keyed by the build's
    /// [`VendorBuildId`].
    ///
    /// Currently constructed only in the merge tests below — the
    /// scheduler dispatch arm that produces it lands in a follow-up
    /// slice (the cross-target `dispatch_vendor_crate` helper). Marked
    /// `allow(dead_code)` to keep the workspace warning-clean during
    /// the gap; remove the attribute when the dispatch arm wires it up.
    #[allow(dead_code)]
    VendorCrate {
        id: VendorBuildId,
        out_path: PathBuf,
        was_cached: bool,
    },
    /// Rule nodes produce no scheduler-visible output and are not
    /// cacheable in MVP-M. The variant exists so per-node arms always
    /// return a `NodeOutput`, keeping the dispatch arm shape uniform.
    Rule,
}

impl ArtifactsSnapshot {
    /// Fold a node's output into the canonical state. Called by the
    /// scheduler dispatch arm under the `Mutex<ArtifactsSnapshot>` lock
    /// — never by per-node helpers. Returns `was_cached` for cacheable
    /// variants so the caller can pick the right counter; `None` for
    /// `Rule` (uncounted in MVP-M).
    pub(crate) fn merge(&mut self, output: NodeOutput) -> Option<bool> {
        match output {
            NodeOutput::Sysroot { target, path, was_cached } => {
                self.sysroots.insert(target, path);
                Some(was_cached)
            }
            NodeOutput::ConfigCrate { target, rlib_path, was_cached } => {
                self.artifacts.set_config_crate(target, rlib_path);
                Some(was_cached)
            }
            NodeOutput::Crate { handle, out_path, was_cached } => {
                self.artifacts.insert(handle, out_path);
                Some(was_cached)
            }
            NodeOutput::Esp { handle, path, was_cached } => {
                self.esps.insert(handle, path);
                Some(was_cached)
            }
            NodeOutput::Image { handle, path, was_cached } => {
                self.images.insert(handle, path);
                Some(was_cached)
            }
            NodeOutput::BuildStep { handle, out_path, was_cached } => {
                self.artifacts.set_build_step(handle, out_path);
                Some(was_cached)
            }
            NodeOutput::VendorCrate { id, out_path, was_cached } => {
                self.vendor_artifacts.insert(id, out_path);
                Some(was_cached)
            }
            NodeOutput::Rule => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_sysroot_inserts_entry_and_reports_cached_flag() {
        let mut s = ArtifactsSnapshot::default();
        let target = Handle::<TargetDef>::new(0);
        let was = s.merge(NodeOutput::Sysroot {
            target,
            path: PathBuf::from("/sys/0"),
            was_cached: true,
        });
        assert_eq!(was, Some(true));
        assert_eq!(
            s.sysroots.get(&target).map(|p| p.as_path()),
            Some(std::path::Path::new("/sys/0"))
        );
    }

    #[test]
    fn merge_rule_returns_none_and_does_not_mutate() {
        let mut s = ArtifactsSnapshot::default();
        let was = s.merge(NodeOutput::Rule);
        assert_eq!(was, None);
        assert!(s.sysroots.is_empty());
        assert!(s.esps.is_empty());
        assert!(s.images.is_empty());
        assert!(s.artifacts.is_empty());
    }

    #[test]
    fn merge_crate_artifact_routes_to_artifact_map_crates_slot() {
        let mut s = ArtifactsSnapshot::default();
        let h = Handle::<CrateDef>::new(7);
        s.merge(NodeOutput::Crate {
            handle: h,
            out_path: PathBuf::from("/out/lib.rlib"),
            was_cached: false,
        });
        assert_eq!(
            s.artifacts.get(h).map(|p| p.to_path_buf()),
            Some(PathBuf::from("/out/lib.rlib")),
        );
    }

    #[test]
    fn merge_vendor_crate_inserts_into_vendor_artifacts_keyed_by_id() {
        let mut s = ArtifactsSnapshot::default();
        // VendorBuildId is opaque; we round-trip via merge → lookup.
        // Build a dummy id by going through a VendorBuildContext add.
        use crate::vendor::cross::context::VendorBuildContext;
        use crate::vendor::cross::graph::VendorCrateBuild;
        use crate::vendor::cross::metadata::{CrateId, CrateKind};
        use gluon_model::TargetSpec;
        let mut ctx = VendorBuildContext::new();
        let id = ctx
            .add(VendorCrateBuild {
                crate_id: CrateId { name: "bitflags".into(), version: "2.11.0".into() },
                cargo_target_name: "bitflags".into(),
                target: TargetSpec::Host,
                kind: CrateKind::Lib,
                manifest_dir: PathBuf::from("/v/bitflags"),
                lib_path: PathBuf::from("/v/bitflags/src/lib.rs"),
                edition: "2021".into(),
                features: vec![],
                deps: vec![],
            })
            .expect("add ok");

        let was = s.merge(NodeOutput::VendorCrate {
            id,
            out_path: PathBuf::from("/out/libbitflags.rlib"),
            was_cached: true,
        });
        assert_eq!(was, Some(true));
        assert_eq!(
            s.vendor_artifacts.get(&id).map(|p| p.as_path()),
            Some(std::path::Path::new("/out/libbitflags.rlib")),
        );
        // Other maps untouched.
        assert!(s.sysroots.is_empty());
        assert!(s.artifacts.is_empty());
    }
}
