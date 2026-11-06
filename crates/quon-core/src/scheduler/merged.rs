//! One schedule for several run entries.
//!
//! Each run entry resolves Cargo features on its own (see
//! `config::select_run`), so each has its own model, unit table and
//! DAG. Building them one after another left most cores idle: a board
//! that adds two crates to what an earlier board built waits for that
//! board to finish. [`execute_entries`] puts every entry's DAG into one
//! and runs it on one worker pool.
//!
//! ### Shared nodes
//!
//! Two entries often contain the same node: the same crate with the
//! same features, the same build-script run, the same sysroot. Such
//! nodes have the same cache key, so they write the same files and the
//! same record, and running them at once would race. The merged DAG
//! therefore has one node per key, a *group*, whose members are the
//! entries' nodes with that key. A group's job runs its members one
//! after another, each through its own entry's dispatcher, so every
//! entry's result slots fill as in a build of that entry alone. The
//! first member does the work; the rest find its fresh record and cost
//! a freshness check. Members that would build differently under one
//! key (which a build of each entry in turn would also do) still run in
//! turn, never at once.
//!
//! A node's key is its fingerprint key where it has one (crates,
//! build-script runs), and otherwise a name that identifies the files
//! it writes: the target for a sysroot, the target and hash for a
//! config crate, the declared name for an ESP, image, rule or build
//! step.
//!
//! A group depends on every group that one of its members depends on.
//! Keys determine a node's inputs, so this adds no cycle.

use super::dag::{Dag, DagNode, NodeId};
use super::worker::{BuildSummary, JobDispatcher, JobInfo, Reporter, WorkerPool};
use super::{PipelineDispatcher, Prepared, counted, node_label};
use crate::compile::CompileCtx;
use crate::error::Result;
use crate::rule::RuleRegistry;
use crate::vendor::cross::context::VendorBuildContext;
use quon_model::{BuildModel, ResolvedConfig};
use std::collections::BTreeMap;

/// One run entry's input to [`execute_entries`].
pub struct EntryPipeline<'a> {
    pub model: &'a BuildModel,
    pub resolved: &'a ResolvedConfig,
    pub vendor_ctx: &'a VendorBuildContext,
}

/// Build every entry in one schedule; see the module docs. The summary
/// counts each group once.
pub fn execute_entries(
    ctx: &CompileCtx,
    entries: &[EntryPipeline<'_>],
    rules: &RuleRegistry,
    workers: usize,
    reporter: &mut dyn Reporter,
) -> Result<BuildSummary> {
    let prepared = entries
        .iter()
        .map(|e| Prepared::new(ctx, e.model, e.resolved, e.vendor_ctx))
        .collect::<Result<Vec<_>>>()?;
    let dispatchers: Vec<PipelineDispatcher<'_>> = entries
        .iter()
        .zip(&prepared)
        .map(|(e, p)| p.dispatcher(ctx, e.model, e.resolved, rules))
        .collect();

    // Entries in order and nodes in id order, so group ids, and with
    // them `-j1` order, are deterministic.
    let mut groups: Vec<Vec<(usize, NodeId)>> = Vec::new();
    let mut nodes: Vec<DagNode> = Vec::new();
    let mut group_of_key: BTreeMap<String, NodeId> = BTreeMap::new();
    let mut group_of: Vec<Vec<NodeId>> = Vec::with_capacity(entries.len());
    for (e, (entry, p)) in entries.iter().zip(&prepared).enumerate() {
        let mut local_groups = Vec::with_capacity(p.dag.len());
        for (local, &node) in p.dag.nodes.iter().enumerate() {
            let local = local as NodeId;
            let key = merge_key(ctx, entry, p, node)?;
            let group = *group_of_key.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                nodes.push(node);
                (groups.len() - 1) as NodeId
            });
            groups[group as usize].push((e, local));
            local_groups.push(group);
        }
        group_of.push(local_groups);
    }

    // `Dag::insert_node` keys nodes by `DagNode`, and two entries'
    // handles can collide, so the merged DAG is assembled directly. The
    // pool reads only `nodes`, `edges_out` and `in_degree`.
    let mut dag = Dag {
        in_degree: (0..nodes.len() as NodeId).map(|id| (id, 0)).collect(),
        nodes,
        edges_out: BTreeMap::new(),
        node_index: BTreeMap::new(),
    };
    for (e, p) in prepared.iter().enumerate() {
        for (&from, tos) in &p.dag.edges_out {
            for &to in tos {
                let (from, to) = (group_of[e][from as usize], group_of[e][to as usize]);
                if from != to {
                    dag.add_edge(from, to);
                }
            }
        }
    }

    let merged = MergedDispatcher {
        groups: &groups,
        dispatchers: &dispatchers,
    };
    WorkerPool::new(workers).execute_with(&dag, &merged, reporter)?;

    // Only a build counts copies: `check` and `clippy` also lower the
    // host tests, whose packages resolve features one by one on
    // purpose, and `quon explain copies` explains a build. Entry names
    // do not change the count, so indices stand in for them.
    let names: Vec<String> = (0..entries.len()).map(|e| e.to_string()).collect();
    let units: Vec<crate::copies::EntryUnits<'_>> = entries
        .iter()
        .zip(&prepared)
        .zip(&names)
        .map(|((e, p), name)| crate::copies::EntryUnits {
            name,
            model: e.model,
            units: &p.units,
        })
        .collect();
    let crates_with_copies = if ctx.driver() == crate::compile::DriverKind::Rustc {
        crate::copies::find(&units).len()
    } else {
        0
    };
    let mut summary = BuildSummary {
        crates_with_copies,
        ..BuildSummary::default()
    };
    for (group, members) in groups.iter().enumerate() {
        let (e, _) = members[0];
        if counted(dag.nodes[group], entries[e].model) {
            if merged.was_cached(group as NodeId) {
                summary.cached += 1;
            } else {
                summary.built += 1;
            }
        }
    }
    Ok(summary)
}

/// The key that merges `node` with other entries' nodes; see the module
/// docs. The kind leads, so keys of different kinds never meet.
fn merge_key(
    ctx: &CompileCtx,
    entry: &EntryPipeline<'_>,
    p: &Prepared,
    node: DagNode,
) -> Result<String> {
    let (model, resolved) = (entry.model, entry.resolved);
    Ok(match node {
        DagNode::Compile(u) => format!(
            "compile\0{}",
            p.units.cache_key(u, &ctx.layout, model, resolved)?
        ),
        DagNode::RunBuildScript(r) => {
            format!("run\0{}", p.units.run_key(r, &ctx.layout, model, resolved)?)
        }
        DagNode::ConfigCrate(t) => format!(
            "config\0{}\0{}",
            node_label(node, model, resolved, &p.units),
            p.units
                .config_metadata(t)
                .map(|m| m.short())
                .unwrap_or_default()
        ),
        DagNode::Sysroot(_)
        | DagNode::Rule(_)
        | DagNode::Esp(_)
        | DagNode::Image(_)
        | DagNode::BuildStep(_) => {
            format!("named\0{}", node_label(node, model, resolved, &p.units))
        }
    })
}

/// Runs a group's members through their entries' dispatchers.
struct MergedDispatcher<'a, 'd> {
    groups: &'a [Vec<(usize, NodeId)>],
    dispatchers: &'a [PipelineDispatcher<'d>],
}

impl JobDispatcher for MergedDispatcher<'_, '_> {
    fn dispatch(&self, node: NodeId, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> Result<()> {
        let members = &self.groups[node as usize];
        let (e, local) = members[0];
        self.dispatchers[e].dispatch(local, stdout, stderr)?;
        for &(e, local) in &members[1..] {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let result = self.dispatchers[e].dispatch(local, &mut out, &mut err);
            // A cache hit replays the warnings the first member already
            // printed; only a member that did work, or failed, has
            // something new to say.
            if result.is_err() || !self.dispatchers[e].was_cached(local) {
                stdout.append(&mut out);
                stderr.append(&mut err);
            }
            result?;
        }
        Ok(())
    }

    fn label(&self, node: NodeId) -> String {
        let (e, local) = self.groups[node as usize][0];
        self.dispatchers[e].label(local)
    }

    fn info(&self, node: NodeId) -> JobInfo {
        let (e, local) = self.groups[node as usize][0];
        self.dispatchers[e].info(local)
    }

    fn was_cached(&self, node: NodeId) -> bool {
        self.groups[node as usize]
            .iter()
            .all(|&(e, local)| self.dispatchers[e].was_cached(local))
    }
}
