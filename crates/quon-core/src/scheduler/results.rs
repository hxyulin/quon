//! Per-node results and the read-only view that dispatch helpers use.
//!
//! Each node writes its own [`NodeResult`] slot exactly once, when it
//! succeeds. A node reads only the slots of its inputs, and the DAG
//! guarantees those are set before the node is dispatched. That makes
//! the slots safe to read without a lock: `OnceLock` publishes the value
//! with release/acquire ordering, and nothing ever writes a slot twice.
//! See `docs/src/architecture/concurrency.md`.

use super::dag::{Dag, DagNode, NodeId};
use crate::build_script::BuildScriptOutput;
use crate::compile::extern_map::ArtifactLookup;
use crate::unit::{RunId, UnitGraph, UnitId};
use quon_model::{BuildStepDef, CrateDef, EspDef, Handle, ImageDef, TargetDef};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Data a node hands to its dependents besides files.
///
/// A `RunBuildScript` node stores its parsed output here and in its
/// fingerprint record, so a cache hit restores it without running the
/// script. A `Compile` node stores the native link-search paths of the
/// build scripts in its closure, so a dependent reads one slot per
/// direct dep instead of walking the graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeData {
    #[default]
    None,
    BuildScript(Arc<BuildScriptOutput>),
    /// `rustc-link-search` values, in dep order then the unit's own,
    /// without duplicates.
    LinkSearch(Arc<[String]>),
}

/// What a finished node hands to its dependents and to the summary.
#[derive(Debug)]
pub(crate) struct NodeResult {
    /// The node's main output: rlib, dylib or binary for crates, the
    /// sysroot directory, the ESP directory, the image file, or the
    /// build-step output. `None` for rule and build-script run nodes.
    pub(crate) primary: Option<PathBuf>,
    /// True when the freshness check skipped the node's action.
    pub(crate) was_cached: bool,
    pub(crate) data: NodeData,
}

/// Read-only lookups over the result slots, keyed the way helpers ask
/// for inputs (crate handle, target, ESP handle, ...).
pub(crate) struct ResultView<'a> {
    dag: &'a Dag,
    results: &'a [OnceLock<NodeResult>],
    units: &'a UnitGraph,
    /// `NodeId` of each unit's `Compile` node, indexed by `UnitId`.
    node_of_unit: &'a [NodeId],
}

/// `NodeId` of each unit's `Compile` node, indexed by `UnitId`.
pub(crate) fn node_of_unit(dag: &Dag, units: &UnitGraph) -> Vec<NodeId> {
    units
        .iter()
        .map(|(id, _)| dag.node_index[&DagNode::Compile(id)])
        .collect()
}

impl<'a> ResultView<'a> {
    pub(crate) fn new(
        dag: &'a Dag,
        results: &'a [OnceLock<NodeResult>],
        units: &'a UnitGraph,
        node_of_unit: &'a [NodeId],
    ) -> Self {
        Self {
            dag,
            results,
            units,
            node_of_unit,
        }
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

    fn unit_result(&self, u: UnitId) -> Option<&'a NodeResult> {
        let node = *self.node_of_unit.get(u.0 as usize)?;
        self.results.get(node as usize)?.get()
    }
}

impl ArtifactLookup for ResultView<'_> {
    fn get(&self, h: Handle<CrateDef>) -> Option<&Path> {
        self.unit(self.units.unit_of(h)?)
    }
    fn unit(&self, u: UnitId) -> Option<&Path> {
        self.unit_result(u)?.primary.as_deref()
    }
    fn build_script(&self, r: RunId) -> Option<&BuildScriptOutput> {
        match &self.node(DagNode::RunBuildScript(r))?.data {
            NodeData::BuildScript(out) => Some(out),
            _ => None,
        }
    }
    fn link_search(&self, u: UnitId) -> &[String] {
        match self.unit_result(u).map(|r| &r.data) {
            Some(NodeData::LinkSearch(paths)) => paths,
            _ => &[],
        }
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

    fn slots(n: usize) -> Vec<OnceLock<NodeResult>> {
        (0..n).map(|_| OnceLock::new()).collect()
    }

    #[test]
    fn view_reads_only_finished_slots() {
        let mut dag = Dag::new();
        let target = Handle::<TargetDef>::new(0);
        let sysroot = dag.insert_node(DagNode::Sysroot(target));
        dag.insert_node(DagNode::Compile(UnitId(0)));
        let results = slots(dag.len());
        results[sysroot as usize]
            .set(NodeResult {
                primary: Some(PathBuf::from("/sys/0")),
                was_cached: true,
                data: NodeData::None,
            })
            .unwrap();

        let units = UnitGraph::default();
        let node_of_unit = [1];
        let view = ResultView::new(&dag, &results, &units, &node_of_unit);
        assert_eq!(view.sysroot(target), Some(Path::new("/sys/0")));
        assert_eq!(view.unit(UnitId(0)), None, "unfinished node has no output");
        assert_eq!(
            view.esp(Handle::new(3)),
            None,
            "node not in the DAG has no output"
        );
    }

    #[test]
    fn view_is_readable_from_parallel_threads() {
        let mut dag = Dag::new();
        let ids: Vec<UnitId> = (0..8).map(UnitId).collect();
        let node_of_unit: Vec<NodeId> = ids
            .iter()
            .map(|&u| dag.insert_node(DagNode::Compile(u)))
            .collect();
        let results = slots(dag.len());
        std::thread::scope(|s| {
            for (i, slot) in results.iter().enumerate() {
                s.spawn(move || {
                    slot.set(NodeResult {
                        primary: Some(PathBuf::from(format!("/out/{i}"))),
                        was_cached: false,
                        data: NodeData::None,
                    })
                    .unwrap();
                });
            }
        });
        let units = UnitGraph::default();
        let view = ResultView::new(&dag, &results, &units, &node_of_unit);
        std::thread::scope(|s| {
            for (i, &u) in ids.iter().enumerate() {
                let view = &view;
                s.spawn(move || {
                    assert_eq!(view.unit(u), Some(Path::new(&format!("/out/{i}"))));
                });
            }
        });
    }
}
