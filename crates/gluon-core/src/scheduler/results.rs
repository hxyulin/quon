//! Per-node results and the read-only view that dispatch helpers use.
//!
//! Each node writes its own [`NodeResult`] slot exactly once, when it
//! succeeds. A node reads only the slots of its inputs, and the DAG
//! guarantees those are set before the node is dispatched. That makes
//! the slots safe to read without a lock: `OnceLock` publishes the value
//! with release/acquire ordering, and nothing ever writes a slot twice.
//! See `docs/src/architecture/concurrency.md`.

use super::dag::{Dag, DagNode};
use crate::compile::extern_map::ArtifactLookup;
use crate::vendor::cross::context::VendorBuildId;
use gluon_model::{BuildStepDef, CrateDef, EspDef, Handle, ImageDef, TargetDef};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// What a finished node hands to its dependents and to the summary.
#[derive(Debug)]
pub(crate) struct NodeResult {
    /// The node's main output: rlib, dylib or binary for crates, the
    /// sysroot directory, the ESP directory, the image file, or the
    /// build-step output. `None` for rule nodes.
    pub(crate) primary: Option<PathBuf>,
    /// True when the freshness check skipped the node's action.
    pub(crate) was_cached: bool,
}

/// Read-only lookups over the result slots, keyed the way helpers ask
/// for inputs (crate handle, target, ESP handle, ...).
pub(crate) struct ResultView<'a> {
    dag: &'a Dag,
    results: &'a [OnceLock<NodeResult>],
}

impl<'a> ResultView<'a> {
    pub(crate) fn new(dag: &'a Dag, results: &'a [OnceLock<NodeResult>]) -> Self {
        Self { dag, results }
    }

    /// The result of `node`, or `None` when the node is not in the DAG
    /// or has not finished.
    pub(crate) fn node(&self, node: DagNode) -> Option<&'a NodeResult> {
        let id = *self.dag.node_index.get(&node)?;
        self.results.get(id as usize)?.get()
    }

    fn primary(&self, node: DagNode) -> Option<&'a Path> {
        self.node(node)?.primary.as_deref()
    }

    /// Sysroot directory for a cross target.
    pub(crate) fn sysroot(&self, target: Handle<TargetDef>) -> Option<&'a Path> {
        self.primary(DagNode::Sysroot(target))
    }

    /// Artifact of a vendored crate build.
    pub(crate) fn vendor(&self, id: VendorBuildId) -> Option<&'a Path> {
        self.primary(DagNode::VendorCrate(id))
    }
}

impl ArtifactLookup for ResultView<'_> {
    fn get(&self, h: Handle<CrateDef>) -> Option<&Path> {
        self.primary(DagNode::Crate(h))
    }
    fn config_crate(&self, target: Handle<TargetDef>) -> Option<&Path> {
        self.primary(DagNode::ConfigCrate(target))
    }
    fn get_build_step(&self, h: Handle<BuildStepDef>) -> Option<&Path> {
        self.primary(DagNode::BuildStep(h))
    }
    fn esp(&self, h: Handle<EspDef>) -> Option<&Path> {
        self.primary(DagNode::Esp(h))
    }
    fn image(&self, h: Handle<ImageDef>) -> Option<&Path> {
        self.primary(DagNode::Image(h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_reads_only_finished_slots() {
        let mut dag = Dag::new();
        let target = Handle::<TargetDef>::new(0);
        let krate = Handle::<CrateDef>::new(0);
        let sysroot = dag.insert_node(DagNode::Sysroot(target));
        dag.insert_node(DagNode::Crate(krate));
        let results: Vec<OnceLock<NodeResult>> = (0..dag.len()).map(|_| OnceLock::new()).collect();
        results[sysroot as usize]
            .set(NodeResult {
                primary: Some(PathBuf::from("/sys/0")),
                was_cached: true,
            })
            .unwrap();

        let view = ResultView::new(&dag, &results);
        assert_eq!(view.sysroot(target), Some(Path::new("/sys/0")));
        assert_eq!(view.get(krate), None, "unfinished node has no output");
        assert_eq!(
            view.esp(Handle::new(3)),
            None,
            "node not in the DAG has no output"
        );
    }

    #[test]
    fn view_is_readable_from_parallel_threads() {
        let mut dag = Dag::new();
        let handles: Vec<Handle<CrateDef>> = (0..8).map(Handle::new).collect();
        for &h in &handles {
            dag.insert_node(DagNode::Crate(h));
        }
        let results: Vec<OnceLock<NodeResult>> = (0..dag.len()).map(|_| OnceLock::new()).collect();
        std::thread::scope(|s| {
            for (i, slot) in results.iter().enumerate() {
                s.spawn(move || {
                    slot.set(NodeResult {
                        primary: Some(PathBuf::from(format!("/out/{i}"))),
                        was_cached: false,
                    })
                    .unwrap();
                });
            }
        });
        let view = ResultView::new(&dag, &results);
        std::thread::scope(|s| {
            for (i, &h) in handles.iter().enumerate() {
                let view = &view;
                s.spawn(move || {
                    assert_eq!(view.get(h), Some(Path::new(&format!("/out/{i}"))));
                });
            }
        });
    }
}
