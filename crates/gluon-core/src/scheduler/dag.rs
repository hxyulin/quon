//! DAG of build nodes and the `build_dag` constructor.
//!
//! The DAG is generic in the sense that the scheduler never looks inside
//! a `DagNode` — it only tracks identities and edges. The pipeline
//! dispatcher is the only code that matches on `DagNode` variant and
//! calls the corresponding work function.
//!
//! Adjacency uses `BTreeMap`/`BTreeSet` for cache-key stability — see
//! `docs/src/architecture/determinism.md`.
//!
//! ### Why `BTreeSet` for `edges_out` values instead of `Vec`?
//!
//! `BTreeSet` provides automatic deduplication of edges. A previous
//! implementation used `Vec` and suffered a real scheduler deadlock: when
//! `add_edge` was called twice for the same (from, to) pair (a common
//! occurrence because multiple rules in a stage can reference the same
//! group), the in-degree of `to` was incremented twice but the edge was
//! only conceptually present once. The result was a node whose in-degree
//! could never reach zero — a permanent deadlock. `BTreeSet` prevents
//! this by construction: inserting the same edge twice is a no-op, and
//! `add_edge` increments in-degree only when the set actually grows.
//!
//! **Do not change `edges_out` to `Vec` under any circumstances.**

use crate::error::{Error, Result};
use crate::unit::{DepTarget, Platform, RunId, UnitGraph, UnitId, UnitOrigin};
use gluon_model::{
    BuildModel, BuildStepDef, BuildStepInput, CrateDef, EspDef, Handle, ImageDef, ImageSource,
    ResolvedConfig, RuleDef, TargetDef,
};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// DagNode
// ---------------------------------------------------------------------------

/// A work node in the scheduler DAG.
///
/// The scheduler treats these as opaque identities — it only tracks edges and
/// in-degrees. The pipeline dispatcher matches on the variant to decide what
/// work to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum DagNode {
    /// Build the custom sysroot for a specific target triple.
    Sysroot(Handle<TargetDef>),
    /// Generate + compile the `<project>_config` crate for a specific target.
    /// One per distinct cross target whose units link it — each target gets
    /// its own config crate rlib so all cross crates can `--extern` it
    /// regardless of which target they compile for.
    ConfigCrate(Handle<TargetDef>),
    /// Compile one build unit: a crate declared in `gluon.rs` or a
    /// vendored crate, for host or for a cross target. See
    /// [`crate::unit::BuildUnit`].
    Compile(UnitId),
    /// Run one build script for one owning unit (see
    /// [`crate::unit::ScriptRun`]). Its parsed output reaches the
    /// owner's rustc command through the node's result data.
    RunBuildScript(RunId),
    /// Run a user-declared rule via the rule registry.
    Rule(Handle<RuleDef>),
    /// Assemble an EFI System Partition directory from the primary build
    /// outputs of one or more sibling crates. Depends on every source
    /// crate referenced by the ESP's entries. See [`EspDef`].
    Esp(Handle<EspDef>),
    /// Assemble a bootable disk image from compiled crate artifacts,
    /// host files, raw byte placements and nested partitions. Depends on
    /// every source crate referenced by the image's entries, and on every
    /// ESP referenced via `ImageSource::Esp`. See [`ImageDef`].
    Image(Handle<ImageDef>),
    /// Invoke a host-bin runner with curated env vars and capture its
    /// single output file as a named artifact. Depends on the runner
    /// crate and on every `BuildStepInput::Crate` it references.
    /// Consumers that look up the build step by name (image entries,
    /// raw writes, qemu `disk_ref`, etc.) get edges *to* this node.
    /// See [`BuildStepDef`].
    BuildStep(Handle<BuildStepDef>),
}

// ---------------------------------------------------------------------------
// NodeId
// ---------------------------------------------------------------------------

/// Compact index into `Dag::nodes`. Cheap to copy; used as the key for all
/// adjacency structures.
pub type NodeId = u32;

// ---------------------------------------------------------------------------
// Dag
// ---------------------------------------------------------------------------

/// A directed acyclic graph of work nodes with deterministic ordering.
///
/// All adjacency structures use `BTreeMap`/`BTreeSet` for stable iteration
/// across runs — see module-level doc for the full rationale.
#[derive(Debug)]
pub struct Dag {
    /// All nodes, indexed by `NodeId`.
    pub nodes: Vec<DagNode>,
    /// Adjacency list: `edges_out[from]` = set of NodeIds that depend on `from`.
    /// `BTreeSet` ensures edge deduplication and deterministic iteration.
    /// See module-level note on why `Vec` is explicitly forbidden here.
    pub edges_out: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// In-degree for each node. Nodes with in-degree 0 are immediately ready.
    pub in_degree: BTreeMap<NodeId, u32>,
    /// Reverse lookup: `DagNode` → its `NodeId`. Ensures `insert_node` is
    /// idempotent — same logical node always maps to the same `NodeId`.
    pub node_index: BTreeMap<DagNode, NodeId>,
}

impl Dag {
    /// Create an empty DAG.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges_out: BTreeMap::new(),
            in_degree: BTreeMap::new(),
            node_index: BTreeMap::new(),
        }
    }

    /// Insert a node, returning its `NodeId`.
    ///
    /// Idempotent: the same `DagNode` inserted twice returns the same `NodeId`
    /// without any side effects. This makes it safe to call `insert_node`
    /// speculatively for dependency nodes without checking for prior insertion.
    pub fn insert_node(&mut self, node: DagNode) -> NodeId {
        if let Some(&id) = self.node_index.get(&node) {
            return id;
        }
        let id = self.nodes.len() as NodeId;
        self.nodes.push(node);
        self.node_index.insert(node, id);
        // Initialise in-degree to 0. This entry must exist for every node so
        // that `ready()` can iterate `in_degree` to find zero-degree nodes.
        self.in_degree.insert(id, 0);
        id
    }

    /// Add an edge: `to` depends on `from`.
    ///
    /// Idempotent via `BTreeSet`: inserting the same edge a second time is a
    /// no-op and does **not** increment the in-degree of `to` a second time.
    /// This is the critical invariant that prevents scheduler deadlocks — see
    /// the module-level doc for the historical bug this guards against.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId) {
        // edges_out[from] is the set of nodes that depend on `from`.
        let inserted = self.edges_out.entry(from).or_default().insert(to);
        // Only increment in-degree when the edge is genuinely new.
        if inserted {
            *self.in_degree.entry(to).or_insert(0) += 1;
        }
    }

    /// Return all `NodeId`s with in-degree 0 at the moment of the call.
    ///
    /// Results are in ascending `NodeId` order (deterministic via `BTreeMap`
    /// key ordering). The caller (worker pool) uses this to seed the initial
    /// ready queue and to determine which nodes become unblocked after a
    /// predecessor completes.
    pub fn ready(&self) -> Vec<NodeId> {
        self.in_degree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect()
    }

    /// Iterate over the successor `NodeId`s of `from` — i.e., nodes that have
    /// `from` as a dependency and become candidates for unblocking when `from`
    /// completes.
    pub fn dependents(&self, from: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.edges_out
            .get(&from)
            .into_iter()
            .flat_map(|set| set.iter().copied())
    }

    /// Total number of nodes in the DAG.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True when the DAG contains no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

impl Default for Dag {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// build_dag
// ---------------------------------------------------------------------------

/// Build the scheduler DAG from a resolved configuration, the build model
/// and the unit table from [`crate::unit::lower`].
///
/// Construction has two passes. The first inserts every node in a fixed
/// order, so `NodeId`s are stable across runs:
///
/// 1. `Sysroot` and `ConfigCrate` for the profile's target (the config
///    crate is unconditional), then for each target a unit links.
/// 2. One `Compile` per unit, in unit order.
/// 3. Every ESP, image and build step, in arena order.
/// 4. One `Rule` per pipeline stage that names a rule.
///
/// The second pass asks [`inputs`] for each node's inputs and adds an edge
/// from each.
///
/// ### Gated crates
///
/// A crate that exists in the model but not in this build (gated out by
/// the profile or config) has no unit. Every lookup by crate handle or
/// name goes through [`UnitGraph::unit_of`], so such a crate is skipped
/// in one place: ESP entries, image entries, build-step inputs and rule
/// groups that name it get no edge. Units only refer to units in the
/// table, so `Compile` edges need no check.
///
/// ### Cycle detection
///
/// NOT performed here. Malformed models that contain cycles will simply fail
/// to make progress at scheduling time (the worker pool detects deadlock and
/// returns an error).
pub fn build_dag(resolved: &ResolvedConfig, model: &BuildModel, units: &UnitGraph) -> Result<Dag> {
    let mut dag = Dag::new();

    let insert_target = |dag: &mut Dag, t: Handle<TargetDef>, config: bool| {
        dag.insert_node(DagNode::Sysroot(t));
        if config {
            dag.insert_node(DagNode::ConfigCrate(t));
        }
    };
    insert_target(&mut dag, resolved.profile.target, true);
    for (_, unit) in units.iter() {
        if let (true, Platform::Cross(t)) = (unit.sysroot, unit.platform) {
            insert_target(&mut dag, t, unit.config_crate);
        }
    }
    for (id, _) in units.iter() {
        dag.insert_node(DagNode::Compile(id));
    }
    for (id, _) in units.runs() {
        dag.insert_node(DagNode::RunBuildScript(id));
    }
    for (h, _) in model.esps.iter() {
        dag.insert_node(DagNode::Esp(h));
    }
    for (h, _) in model.images.iter() {
        dag.insert_node(DagNode::Image(h));
    }
    for (h, _) in model.build_steps.iter() {
        dag.insert_node(DagNode::BuildStep(h));
    }
    for (_, pipeline) in model.pipelines.iter() {
        for step in &pipeline.stages {
            let Some(rule_name) = &step.rule else {
                continue;
            };
            let rule = model.rules.lookup(rule_name).ok_or_else(|| {
                Error::Compile(format!(
                    "build_dag: pipeline '{}' stage '{}' references unknown rule '{}'",
                    pipeline.name, step.name, rule_name
                ))
            })?;
            dag.insert_node(DagNode::Rule(rule));
        }
    }

    for to in 0..dag.len() as NodeId {
        for input in inputs(dag.nodes[to as usize], units, model) {
            let from = dag.insert_node(input);
            dag.add_edge(from, to);
        }
    }
    Ok(dag)
}

/// The nodes `node` reads outputs from, or must run after.
///
/// | Node | Inputs |
/// |---|---|
/// | `Compile(u)` | `Sysroot(t)` if the unit links the sysroot; `ConfigCrate(t)` if it links the config crate; `Compile` of each dep unit and each order dep (`artifact_deps`); `RunBuildScript(r)` if the unit has a build-script run |
/// | `RunBuildScript(r)` | `Compile` of the script; `Sysroot(t)` when the owner is a cross unit that links the sysroot, because probe-style scripts compile against it |
/// | `ConfigCrate(t)` | `Sysroot(t)` |
/// | `Esp(e)` | `Compile` of each entry's source crate |
/// | `Image(i)` | per entry and raw write: crate name → `Compile`, else build-step name → `BuildStep`; ESP name → `Esp`. Host files have no node |
/// | `BuildStep(s)` | `Compile` of the runner and of each crate input |
/// | `Rule(r)` | the rule of the previous rule-bearing stage in each pipeline that uses `r`, and `Compile` of every declared crate in the stage's input groups |
/// | `Sysroot(t)` | none |
///
/// Vendored crates are units like any other: a declared crate's edge to
/// a vendored dep is a `Compile` → `Compile` edge, resolved in
/// [`crate::unit::lower`]. Vendored units link the sysroot but not the
/// config crate.
pub fn inputs(node: DagNode, units: &UnitGraph, model: &BuildModel) -> Vec<DagNode> {
    let compile_of = |h: Handle<CrateDef>| units.unit_of(h).map(DagNode::Compile);
    // Names are looked up in the unified artifact namespace: a crate
    // first, then a build step. Validate rejects name collisions, so at
    // most one resolves. A crate that exists but is gated out of this
    // build gets no edge and does not fall back to a build step.
    let by_name = |name: &str| -> Option<DagNode> {
        match model.crates.lookup(name) {
            Some(h) => compile_of(h),
            None => model.build_steps.lookup(name).map(DagNode::BuildStep),
        }
    };
    match node {
        DagNode::Sysroot(_) => Vec::new(),
        DagNode::ConfigCrate(t) => vec![DagNode::Sysroot(t)],
        DagNode::Compile(id) => {
            let unit = units.get(id);
            let mut out = Vec::new();
            if let Platform::Cross(t) = unit.platform {
                if unit.sysroot {
                    out.push(DagNode::Sysroot(t));
                }
                if unit.config_crate {
                    out.push(DagNode::ConfigCrate(t));
                }
            }
            for dep in &unit.deps {
                if let DepTarget::Unit(d) = dep.target {
                    out.push(DagNode::Compile(d));
                }
            }
            out.extend(unit.order_deps.iter().map(|&d| DagNode::Compile(d)));
            out.extend(units.build_script_of(id).map(DagNode::RunBuildScript));
            out
        }
        DagNode::RunBuildScript(r) => {
            let run = units.run(r);
            let mut out = vec![DagNode::Compile(run.script)];
            let owner = units.get(run.owner);
            if let (true, Platform::Cross(t)) = (owner.sysroot, owner.platform) {
                out.push(DagNode::Sysroot(t));
            }
            out
        }
        DagNode::Esp(h) => model
            .esps
            .get(h)
            .map(|esp| {
                esp.entries
                    .iter()
                    .filter_map(|e| compile_of(e.source_crate))
                    .collect()
            })
            .unwrap_or_default(),
        DagNode::Image(h) => {
            let Some(image) = model.images.get(h) else {
                return Vec::new();
            };
            let entries = image
                .entries
                .iter()
                .chain(image.partitions.iter().flat_map(|p| p.entries.iter()));
            let mut out: Vec<DagNode> = entries
                .filter_map(|entry| match &entry.source {
                    ImageSource::Crate(name) => by_name(name),
                    ImageSource::Esp(name) => model.esps.lookup(name).map(DagNode::Esp),
                    // Host files exist before gluon runs; the image node
                    // checks their freshness itself.
                    ImageSource::File(_) => None,
                })
                .collect();
            // Raw writes of compiled crates (a boot sector or stage blob
            // built as `raw_binary`). File and byte sources have no node.
            out.extend(image.raw_writes.iter().filter_map(|rw| match &rw.source {
                gluon_model::RawSource::Crate { name } => by_name(name),
                _ => None,
            }));
            out
        }
        DagNode::BuildStep(h) => {
            let Some(step) = model.build_steps.get(h) else {
                return Vec::new();
            };
            let mut out: Vec<DagNode> = compile_of(step.runner).into_iter().collect();
            out.extend(step.inputs.values().filter_map(|input| match input {
                BuildStepInput::Crate { handle } => compile_of(*handle),
                _ => None,
            }));
            out
        }
        DagNode::Rule(r) => {
            let mut out = Vec::new();
            for (_, pipeline) in model.pipelines.iter() {
                // Stages without a rule are skipped and keep the previous
                // rule as the barrier for the next rule-bearing stage.
                let mut prev: Option<Handle<RuleDef>> = None;
                for step in &pipeline.stages {
                    let Some(rule) = step.rule.as_deref().and_then(|n| model.rules.lookup(n))
                    else {
                        continue;
                    };
                    if rule == r {
                        out.extend(prev.map(DagNode::Rule));
                        for (id, unit) in units.iter() {
                            if let UnitOrigin::Crate(h) = unit.origin
                                && model
                                    .crates
                                    .get(h)
                                    .is_some_and(|c| step.inputs.contains(&c.group))
                            {
                                out.push(DagNode::Compile(id));
                            }
                        }
                    }
                    prev = Some(rule);
                }
            }
            out
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use gluon_model::DepResolution;
    use gluon_model::{
        BuildModel, CrateDef, CrateType, DepDef, GroupDef, PipelineDef, PipelineStep, ProjectDef,
        ResolvedCrateRef, RuleDef, RuleHandler, TargetDef, TargetSpec,
    };

    // -----------------------------------------------------------------------
    // Test-support helpers
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod test_support {
        use super::*;
        use crate::vendor::cross::context::{VendorBuildContext, VendorBuildId};

        /// A built DAG plus the unit table it was built from, so tests
        /// can name `Compile` nodes by crate handle or vendor build.
        #[derive(Debug)]
        pub struct TestDag {
            pub dag: Dag,
            pub units: UnitGraph,
        }

        impl std::ops::Deref for TestDag {
            type Target = Dag;
            fn deref(&self) -> &Dag {
                &self.dag
            }
        }

        impl TestDag {
            /// The `Compile` node of a declared crate. Panics when the
            /// crate has no unit (not in `resolved.crates`).
            pub fn crate_node(&self, h: Handle<CrateDef>) -> DagNode {
                DagNode::Compile(self.units.unit_of(h).expect("crate has a unit"))
            }

            pub fn vendor_node(&self, id: VendorBuildId) -> DagNode {
                DagNode::Compile(
                    self.units
                        .unit_of_vendor(id)
                        .expect("vendor build has a unit"),
                )
            }
        }

        /// Lower the units and build the DAG, as `execute_pipeline` does.
        pub fn test_dag(
            resolved: &ResolvedConfig,
            model: &BuildModel,
            vendor_ctx: &VendorBuildContext,
        ) -> Result<TestDag> {
            let units = crate::unit::lower(model, resolved, vendor_ctx)?;
            let dag = build_dag(resolved, model, &units)?;
            Ok(TestDag { dag, units })
        }
        use gluon_model::{ResolvedConfig, ResolvedProfile};
        use std::path::PathBuf;

        pub fn make_target(model: &mut BuildModel, name: &str) -> Handle<TargetDef> {
            let (h, _) = model.targets.insert(
                name.into(),
                TargetDef {
                    name: name.into(),
                    spec: format!("{}-unknown-none", name),
                    builtin: true,
                    panic_strategy: None,
                    span: None,
                },
            );
            h
        }

        pub fn make_profile(target: Handle<TargetDef>) -> ResolvedProfile {
            ResolvedProfile {
                name: "debug".into(),
                target,
                opt_level: 0,
                debug_info: false,
                lto: None,
                boot_binary: None,
                qemu_memory: None,
                qemu_cores: None,
                qemu_extra_args: Vec::new(),
                qemu_boot_mode: None,
                test_timeout: None,
            }
        }

        pub fn make_resolved(
            target_handle: Handle<TargetDef>,
            crates: Vec<ResolvedCrateRef>,
        ) -> ResolvedConfig {
            ResolvedConfig {
                project: ProjectDef {
                    name: "testproject".into(),
                    version: "0.1.0".into(),
                    config_crate_name: None,
                    cfg_prefix: None,
                    config_override_file: None,
                    default_profile: None,
                },
                profile: make_profile(target_handle),
                options: Default::default(),
                crates,
                build_dir: PathBuf::from("/tmp/test-build"),
                project_root: PathBuf::from("/tmp/test"),
            }
        }

        /// Ensure a group named `group_name` exists in `model`. The
        /// group's target is `Cross(target)` if `target.is_some()`,
        /// otherwise `Host`. Returns the resolved handle.
        pub fn ensure_group(
            model: &mut BuildModel,
            group_name: &str,
            target: Option<Handle<TargetDef>>,
        ) -> Handle<GroupDef> {
            if let Some(h) = model.groups.lookup(group_name) {
                return h;
            }
            let target_spec = match target {
                Some(t) => TargetSpec::Cross(t),
                None => TargetSpec::Host,
            };
            let (h, _) = model.groups.insert(
                group_name.into(),
                GroupDef {
                    name: group_name.into(),
                    target: target_spec,
                    ..Default::default()
                },
            );
            h
        }

        /// Default values for newly-mandatory `CrateDef` fields.
        pub fn test_crate_defaults(group: Handle<GroupDef>) -> CrateDef {
            CrateDef {
                name: String::new(),
                path: String::new(),
                edition: "2024".into(),
                crate_type: CrateType::Lib,
                target: TargetSpec::Host,
                deps: std::collections::BTreeMap::new(),
                dev_deps: std::collections::BTreeMap::new(),
                features: Vec::new(),
                root: None,
                linker_script: None,
                group,
                is_project_crate: true,
                cfg_flags: Vec::new(),
                rustc_flags: Vec::new(),
                requires_config: Vec::new(),
                artifact_deps: Vec::new(),
                artifact_env: std::collections::BTreeMap::new(),
                span: None,
                cargo: None,
            }
        }

        pub fn make_cross_crate(
            model: &mut BuildModel,
            name: &str,
            target_handle: Handle<TargetDef>,
        ) -> Handle<CrateDef> {
            let group = ensure_group(model, "default", Some(target_handle));
            let (h, _) = model.crates.insert(
                name.into(),
                CrateDef {
                    name: name.into(),
                    path: name.into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: TargetSpec::Cross(target_handle),
                    ..test_crate_defaults(group)
                },
            );
            h
        }

        pub fn make_host_crate(model: &mut BuildModel, name: &str) -> Handle<CrateDef> {
            let group = ensure_group(model, "host-group", None);
            let (h, _) = model.crates.insert(
                name.into(),
                CrateDef {
                    name: name.into(),
                    path: name.into(),
                    edition: "2021".into(),
                    crate_type: CrateType::ProcMacro,
                    target: TargetSpec::Host,
                    ..test_crate_defaults(group)
                },
            );
            h
        }
    }

    use test_support::*;

    // -----------------------------------------------------------------------
    // Test 1: insert_node_dedups
    // -----------------------------------------------------------------------
    #[test]
    fn insert_node_dedups() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let node = DagNode::Sysroot(th);

        let id1 = dag.insert_node(node);
        let id2 = dag.insert_node(node);
        assert_eq!(
            id1, id2,
            "same DagNode inserted twice must yield same NodeId"
        );
        assert_eq!(dag.len(), 1, "DAG must have exactly one node after dedup");
    }

    // -----------------------------------------------------------------------
    // Test 2: add_edge_is_idempotent_and_increments_in_degree_once
    // -----------------------------------------------------------------------
    #[test]
    fn add_edge_is_idempotent_and_increments_in_degree_once() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let a = dag.insert_node(DagNode::Sysroot(th));
        let b = dag.insert_node(DagNode::ConfigCrate(th));

        // First insertion: in-degree of b becomes 1.
        dag.add_edge(a, b);
        assert_eq!(
            *dag.in_degree.get(&b).unwrap(),
            1,
            "first add_edge must set in-degree to 1"
        );

        // Second insertion: must be a no-op — in-degree stays 1.
        dag.add_edge(a, b);
        assert_eq!(
            *dag.in_degree.get(&b).unwrap(),
            1,
            "duplicate add_edge must NOT increment in-degree a second time"
        );

        // The edge set has exactly one entry.
        let out = dag.edges_out.get(&a).unwrap();
        assert_eq!(
            out.len(),
            1,
            "BTreeSet must contain exactly one edge after two identical inserts"
        );
    }

    // -----------------------------------------------------------------------
    // Test 3: ready_returns_zero_in_degree_nodes_in_ascending_order
    // -----------------------------------------------------------------------
    #[test]
    fn ready_returns_zero_in_degree_nodes_in_ascending_order() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let ch1 = test_support::make_cross_crate(&mut model, "crate-a", th);
        let ch2 = test_support::make_cross_crate(&mut model, "crate-b", th);

        // Insert three nodes with no edges — all have in-degree 0.
        let id0 = dag.insert_node(DagNode::Sysroot(th));
        let _ = (ch1, ch2);
        let id1 = dag.insert_node(DagNode::Compile(UnitId(0)));
        let id2 = dag.insert_node(DagNode::Compile(UnitId(1)));

        let ready = dag.ready();
        assert_eq!(
            ready,
            vec![id0, id1, id2],
            "ready() must return in ascending NodeId order"
        );
    }

    // -----------------------------------------------------------------------
    // Test 4: ready_excludes_nodes_with_nonzero_in_degree
    // -----------------------------------------------------------------------
    #[test]
    fn ready_excludes_nodes_with_nonzero_in_degree() {
        let mut dag = Dag::new();
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let sysroot = dag.insert_node(DagNode::Sysroot(th));
        let config = dag.insert_node(DagNode::ConfigCrate(th));
        dag.add_edge(sysroot, config);

        let ready = dag.ready();
        assert_eq!(
            ready,
            vec![sysroot],
            "only the sysroot node has in-degree 0"
        );
        assert!(
            !ready.contains(&config),
            "config depends on sysroot and must not be ready"
        );
    }

    // -----------------------------------------------------------------------
    // Test 5: build_dag_minimal_project
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_minimal_project() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let ch = make_cross_crate(&mut model, "my-crate", th);

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: ch,
                target: th,
                host: false,
            }],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        // All three node kinds must be present.
        let sysroot_id = *dag
            .node_index
            .get(&DagNode::Sysroot(th))
            .expect("Sysroot node");
        let config_id = *dag
            .node_index
            .get(&DagNode::ConfigCrate(th))
            .expect("ConfigCrate node");
        let crate_id = *dag.node_index.get(&dag.crate_node(ch)).expect("Crate node");

        assert_eq!(dag.len(), 3);

        // Edge: Sysroot → ConfigCrate.
        let sysroot_outs = dag.edges_out.get(&sysroot_id).expect("sysroot has outs");
        assert!(
            sysroot_outs.contains(&config_id),
            "Sysroot → ConfigCrate edge missing"
        );
        // Edge: Sysroot → Crate.
        assert!(
            sysroot_outs.contains(&crate_id),
            "Sysroot → Crate edge missing"
        );

        // Edge: ConfigCrate → Crate.
        let config_outs = dag.edges_out.get(&config_id).expect("config has outs");
        assert!(
            config_outs.contains(&crate_id),
            "ConfigCrate → Crate edge missing"
        );

        // In-degrees.
        assert_eq!(*dag.in_degree.get(&sysroot_id).unwrap(), 0);
        assert_eq!(*dag.in_degree.get(&config_id).unwrap(), 1);
        assert_eq!(*dag.in_degree.get(&crate_id).unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // Test 6: build_dag_with_host_proc_macro
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_with_host_proc_macro() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        // Host proc-macro crate.
        let host_h = make_host_crate(&mut model, "my-proc-macro");
        // Cross crate that depends on the proc-macro.
        let __group_12 = ensure_group(&mut model, "default", Some(th));
        let cross_h = model
            .crates
            .insert(
                "my-cross-crate".into(),
                CrateDef {
                    name: "my-cross-crate".into(),
                    path: "my-cross-crate".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: TargetSpec::Cross(th),
                    group: __group_12,
                    deps: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "my_proc_macro".into(),
                            DepDef {
                                resolution: DepResolution::Project(host_h),
                                features: Vec::new(),
                                version: None,
                                optional: false,
                                default_features: true,
                                span: None,
                            },
                        );
                        m
                    },
                    ..test_crate_defaults(__group_12)
                },
            )
            .0;

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: host_h,
                    target: th,
                    host: true,
                },
                ResolvedCrateRef {
                    handle: cross_h,
                    target: th,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        let sysroot_id = *dag.node_index.get(&DagNode::Sysroot(th)).unwrap();
        let config_id = *dag.node_index.get(&DagNode::ConfigCrate(th)).unwrap();
        let host_id = *dag.node_index.get(&dag.crate_node(host_h)).unwrap();
        let cross_id = *dag.node_index.get(&dag.crate_node(cross_h)).unwrap();

        // Host crate must NOT depend on Sysroot or ConfigCrate.
        let sysroot_outs = dag.edges_out.get(&sysroot_id).unwrap();
        assert!(
            !sysroot_outs.contains(&host_id),
            "host proc-macro must NOT depend on the cross sysroot"
        );
        let config_outs = dag.edges_out.get(&config_id).unwrap();
        assert!(
            !config_outs.contains(&host_id),
            "host proc-macro must NOT depend on ConfigCrate"
        );

        // Cross crate depends on Sysroot, ConfigCrate, and the host crate.
        assert!(
            sysroot_outs.contains(&cross_id),
            "cross crate must depend on Sysroot"
        );
        assert!(
            config_outs.contains(&cross_id),
            "cross crate must depend on ConfigCrate"
        );
        let host_outs = dag
            .edges_out
            .get(&host_id)
            .expect("host proc-macro has outs");
        assert!(
            host_outs.contains(&cross_id),
            "cross crate must depend on host proc-macro"
        );

        // Sanity: host crate's in-degree is 0 (no predecessors).
        assert_eq!(*dag.in_degree.get(&host_id).unwrap(), 0);
        // Cross crate's in-degree: Sysroot + ConfigCrate + host = 3.
        assert_eq!(*dag.in_degree.get(&cross_id).unwrap(), 3);
    }

    // -----------------------------------------------------------------------
    // Test 7: build_dag_with_intra_chain_dep
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_with_intra_chain_dep() {
        // A depends on B depends on C (all cross). No transitive shortcut.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let c_h = make_cross_crate(&mut model, "crate-c", th);
        let __group_11 = ensure_group(&mut model, "default", Some(th));
        let b_h = model
            .crates
            .insert(
                "crate-b".into(),
                CrateDef {
                    name: "crate-b".into(),
                    path: "crate-b".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: TargetSpec::Cross(th),
                    group: __group_11,
                    deps: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "crate_c".into(),
                            DepDef {
                                resolution: DepResolution::Project(c_h),
                                features: Vec::new(),
                                version: None,
                                optional: false,
                                default_features: true,
                                span: None,
                            },
                        );
                        m
                    },
                    ..test_crate_defaults(__group_11)
                },
            )
            .0;
        let __group_10 = ensure_group(&mut model, "default", Some(th));
        let a_h = model
            .crates
            .insert(
                "crate-a".into(),
                CrateDef {
                    name: "crate-a".into(),
                    path: "crate-a".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Lib,
                    target: TargetSpec::Cross(th),
                    group: __group_10,
                    deps: {
                        let mut m = BTreeMap::new();
                        m.insert(
                            "crate_b".into(),
                            DepDef {
                                resolution: DepResolution::Project(b_h),
                                features: Vec::new(),
                                version: None,
                                optional: false,
                                default_features: true,
                                span: None,
                            },
                        );
                        m
                    },
                    ..test_crate_defaults(__group_10)
                },
            )
            .0;

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: c_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: b_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: a_h,
                    target: th,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        let c_id = *dag.node_index.get(&dag.crate_node(c_h)).unwrap();
        let b_id = *dag.node_index.get(&dag.crate_node(b_h)).unwrap();
        let a_id = *dag.node_index.get(&dag.crate_node(a_h)).unwrap();

        // C → B edge.
        let c_outs = dag.edges_out.get(&c_id).unwrap();
        assert!(c_outs.contains(&b_id), "C → B edge missing");
        // B → A edge.
        let b_outs = dag.edges_out.get(&b_id).unwrap();
        assert!(b_outs.contains(&a_id), "B → A edge missing");
        // No shortcut C → A.
        assert!(!c_outs.contains(&a_id), "unexpected shortcut C → A");
    }

    // -----------------------------------------------------------------------
    // Test 8: build_dag_with_pipeline_stage_barrier
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_with_pipeline_stage_barrier() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        // Two rules.
        let (rule0_h, _) = model.rules.insert(
            "stage0-rule".into(),
            RuleDef {
                name: "stage0-rule".into(),
                inputs: vec![],
                outputs: vec![],
                depends_on: vec![],
                handler: RuleHandler::Builtin("exec".into()),
                working_dir: None,
                span: None,
            },
        );
        let (rule1_h, _) = model.rules.insert(
            "stage1-rule".into(),
            RuleDef {
                name: "stage1-rule".into(),
                inputs: vec![],
                outputs: vec![],
                depends_on: vec![],
                handler: RuleHandler::Builtin("exec".into()),
                working_dir: None,
                span: None,
            },
        );

        // Pipeline with 2 stages.
        model.pipelines.insert(
            "my-pipeline".into(),
            PipelineDef {
                name: "my-pipeline".into(),
                stages: vec![
                    PipelineStep {
                        name: "stage0".into(),
                        inputs: vec![],
                        rule: Some("stage0-rule".into()),
                    },
                    PipelineStep {
                        name: "stage1".into(),
                        inputs: vec![],
                        rule: Some("stage1-rule".into()),
                    },
                ],
                span: None,
            },
        );

        let resolved = make_resolved(th, vec![]);
        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        let rule0_id = *dag.node_index.get(&DagNode::Rule(rule0_h)).unwrap();
        let rule1_id = *dag.node_index.get(&DagNode::Rule(rule1_h)).unwrap();

        // Stage-1 rule depends on stage-0 rule.
        let rule0_outs = dag
            .edges_out
            .get(&rule0_id)
            .expect("stage0 rule has successors");
        assert!(
            rule0_outs.contains(&rule1_id),
            "stage-1 rule must depend on stage-0 rule"
        );
        // In-degree of stage-0 rule: 0.
        assert_eq!(*dag.in_degree.get(&rule0_id).unwrap(), 0);
        // In-degree of stage-1 rule: 1 (from stage-0 rule).
        assert_eq!(*dag.in_degree.get(&rule1_id).unwrap(), 1);
    }

    // -----------------------------------------------------------------------
    // Test 9: build_dag_rule_depends_on_group_crates
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_rule_depends_on_group_crates() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        // A group with two crates.
        let (group_h, _) = model.groups.insert(
            "kernel-group".into(),
            GroupDef {
                name: "kernel-group".into(),
                target: TargetSpec::Cross(th),
                default_edition: "2021".into(),
                crates: vec!["crate-x".into(), "crate-y".into()],
                ..Default::default()
            },
        );

        // Two cross crates with group_handle set.
        let (cx_h, _) = model.crates.insert(
            "crate-x".into(),
            CrateDef {
                name: "crate-x".into(),
                path: "crate-x".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: TargetSpec::Cross(th),
                group: group_h,
                ..test_crate_defaults(group_h)
            },
        );
        let (cy_h, _) = model.crates.insert(
            "crate-y".into(),
            CrateDef {
                name: "crate-y".into(),
                path: "crate-y".into(),
                edition: "2021".into(),
                crate_type: CrateType::Lib,
                target: TargetSpec::Cross(th),
                group: group_h,
                ..test_crate_defaults(group_h)
            },
        );

        // A rule.
        let (rule_h, _) = model.rules.insert(
            "link-rule".into(),
            RuleDef {
                name: "link-rule".into(),
                inputs: vec![],
                outputs: vec![],
                depends_on: vec![],
                handler: RuleHandler::Builtin("exec".into()),
                working_dir: None,
                span: None,
            },
        );

        // Pipeline with one stage that references the group.
        model.pipelines.insert(
            "link-pipeline".into(),
            PipelineDef {
                name: "link-pipeline".into(),
                stages: vec![PipelineStep {
                    name: "link".into(),
                    inputs: vec![group_h],
                    rule: Some("link-rule".into()),
                }],
                span: None,
            },
        );

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: cx_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: cy_h,
                    target: th,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        let rule_id = *dag.node_index.get(&DagNode::Rule(rule_h)).unwrap();
        let cx_id = *dag.node_index.get(&dag.crate_node(cx_h)).unwrap();
        let cy_id = *dag.node_index.get(&dag.crate_node(cy_h)).unwrap();

        // Both crates' outs must contain the rule.
        let cx_outs = dag.edges_out.get(&cx_id).expect("crate-x has outs");
        assert!(cx_outs.contains(&rule_id), "rule must depend on crate-x");
        let cy_outs = dag.edges_out.get(&cy_id).expect("crate-y has outs");
        assert!(cy_outs.contains(&rule_id), "rule must depend on crate-y");

        // In-degree of rule: both crates feed into it (plus 0 from stage barrier).
        assert_eq!(*dag.in_degree.get(&rule_id).unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // Test 10: artifact_dep_across_groups_and_targets
    // -----------------------------------------------------------------------
    //
    // A bootloader crate (x86_64-unknown-uefi, "uefi-group") has an
    // `artifact_deps` entry pointing at a kernel crate (x86_64-unknown-none,
    // "kernel-group"). The DAG builder must emit a cross-group, cross-target
    // ordering edge from kernel → bootloader even though the two crates
    // share no regular `deps` relationship.
    //
    // This is the load-bearing case for `gluon run`-time artifact embedding
    // (a bootloader that `include_bytes!`es a kernel via a `KERNEL_PATH`
    // env var injected at rustc invocation time). If the edge is missing,
    // the scheduler may run the bootloader node before the kernel is built,
    // and `include_bytes!` will either fail or silently embed stale bytes.
    #[test]
    fn artifact_dep_across_groups_and_targets() {
        let mut model = BuildModel::default();
        let kernel_target = make_target(&mut model, "x86_64-kernel");
        let uefi_target = make_target(&mut model, "x86_64-uefi");

        // Kernel crate in kernel-group / x86_64-kernel target.
        let __group_9 = ensure_group(&mut model, "kernel-group", Some(kernel_target));
        let (kernel_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(kernel_target),
                group: __group_9,
                ..test_crate_defaults(__group_9)
            },
        );

        // Bootloader crate in uefi-group / x86_64-uefi target, with an
        // artifact_dep on the kernel (no regular `deps` entry).
        let __group_8 = ensure_group(&mut model, "uefi-group", Some(uefi_target));
        let (bootloader_h, _) = model.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(uefi_target),
                group: __group_8,
                artifact_deps: vec!["kernel".into()],
                ..test_crate_defaults(__group_8)
            },
        );

        // Resolved config pins the profile to the uefi target (the
        // "running" target) but still lists both crates — the scheduler
        // builds both regardless of which target the profile points at.
        let resolved = make_resolved(
            uefi_target,
            vec![
                ResolvedCrateRef {
                    handle: kernel_h,
                    target: kernel_target,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: bootloader_h,
                    target: uefi_target,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        let kernel_id = *dag
            .node_index
            .get(&dag.crate_node(kernel_h))
            .expect("kernel node present");
        let bootloader_id = *dag
            .node_index
            .get(&dag.crate_node(bootloader_h))
            .expect("bootloader node present");

        // The critical assertion: kernel → bootloader edge exists.
        let kernel_outs = dag
            .edges_out
            .get(&kernel_id)
            .expect("kernel has outgoing edges (at least to bootloader)");
        assert!(
            kernel_outs.contains(&bootloader_id),
            "artifact_deps must produce an ordering edge: kernel → bootloader"
        );

        // Bootloader's in-degree accounts for: Sysroot + ConfigCrate + kernel = 3.
        assert_eq!(
            *dag.in_degree.get(&bootloader_id).unwrap(),
            3,
            "bootloader in-degree must include the artifact-dep edge from kernel"
        );
    }

    // -----------------------------------------------------------------------
    // Test: build_dag_inserts_esp_nodes_with_source_crate_edges
    // -----------------------------------------------------------------------
    //
    // A single EspDef with one entry (source_crate = "bootloader") must
    // produce a `DagNode::Esp` node with an incoming edge from the
    // bootloader crate's `Compile` node. The esp's `entries`
    // must have been interned with a `source_crate_handle` before
    // `build_dag` runs — we simulate that by setting it explicitly.
    #[test]
    fn build_dag_inserts_esp_nodes_with_source_crate_edges() {
        use gluon_model::{EspDef, EspEntry};

        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-uefi");

        let __group_7 = ensure_group(&mut model, "uefi", Some(th));
        let (bl_h, _) = model.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                group: __group_7,
                ..test_crate_defaults(__group_7)
            },
        );

        // Mirror what `intern_esps` would do: resolve the source_crate
        // name to a handle before calling build_dag.
        let (esp_h, _) = model.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![EspEntry {
                    source_crate: bl_h,
                    dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                }],
                span: None,
            },
        );

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: bl_h,
                target: th,
                host: false,
            }],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        let bl_id = *dag.node_index.get(&dag.crate_node(bl_h)).unwrap();
        let esp_id = *dag
            .node_index
            .get(&DagNode::Esp(esp_h))
            .expect("Esp node must be present");

        // bootloader → esp edge.
        let bl_outs = dag.edges_out.get(&bl_id).unwrap();
        assert!(
            bl_outs.contains(&esp_id),
            "bootloader → esp edge missing — ESP would run before the bootloader is built"
        );

        // ESP's in-degree: 1 (the bootloader). Sysroot/ConfigCrate feed
        // the bootloader, not the ESP directly.
        assert_eq!(*dag.in_degree.get(&esp_id).unwrap(), 1);
    }

    // -----------------------------------------------------------------------
    // Test 11: artifact_dep_unknown_name_is_silently_skipped
    // -----------------------------------------------------------------------
    //
    // A dangling `artifact_deps` entry (pointing at a crate name that does
    // not exist in the model) must NOT cause `build_dag` to error. Surfacing
    // that diagnostic is validate.rs's job — this function's contract is
    // to build edges for resolvable entries and skip the rest.
    #[test]
    fn artifact_dep_unknown_name_is_silently_skipped() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");
        let __group_6 = ensure_group(&mut model, "default", Some(th));
        let (consumer_h, _) = model.crates.insert(
            "consumer".into(),
            CrateDef {
                name: "consumer".into(),
                path: "consumer".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                group: __group_6,
                artifact_deps: vec!["ghost".into()],
                ..test_crate_defaults(__group_6)
            },
        );

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: consumer_h,
                target: th,
                host: false,
            }],
        );

        // Must not return Err.
        let dag = test_dag(&resolved, &model, &VendorBuildContext::new())
            .expect("unknown artifact_dep must not abort");
        // Consumer's in-degree: Sysroot + ConfigCrate only (no artifact edge).
        let consumer_id = *dag.node_index.get(&dag.crate_node(consumer_h)).unwrap();
        assert_eq!(*dag.in_degree.get(&consumer_id).unwrap(), 2);
    }

    // -----------------------------------------------------------------------
    // Test: Image DAG node depends on its source crates and ESPs
    // -----------------------------------------------------------------------
    //
    // An ImageDef with one Crate-source entry and one Esp-source entry
    // must produce:
    //   - exactly one DagNode::Image,
    //   - an edge from the source Crate to the Image,
    //   - an edge from the source Esp to the Image,
    //   - and the Image must NOT directly depend on the ESP's source
    //     crates (that's transitive through Esp → Image).
    //
    // ImageSource::File entries produce no edges — they're host files
    // outside the build graph.
    #[test]
    fn build_dag_image_node_wires_crate_and_esp_sources() {
        use gluon_model::{EspDef, EspEntry, ImageEntry, ImageSource as ModelImageSource};

        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-unknown-uefi");

        // Bootloader crate the ESP will host.
        let __group_5 = ensure_group(&mut model, "uefi", Some(th));
        let (bl_h, _) = model.crates.insert(
            "bootloader".into(),
            CrateDef {
                name: "bootloader".into(),
                path: "crates/bootloader".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                group: __group_5,
                ..test_crate_defaults(__group_5)
            },
        );

        // Kernel crate the image packs directly (no ESP indirection).
        let __group_4 = ensure_group(&mut model, "uefi", Some(th));
        let (kernel_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                group: __group_4,
                ..test_crate_defaults(__group_4)
            },
        );

        // ESP that grafts the bootloader.
        let (esp_h, _) = model.esps.insert(
            "default".into(),
            EspDef {
                name: "default".into(),
                entries: vec![EspEntry {
                    source_crate: bl_h,
                    dest_path: "EFI/BOOT/BOOTX64.EFI".into(),
                }],
                span: None,
            },
        );

        // Image that packs both the ESP and the kernel.
        let (img_h, _) = model.images.insert(
            "boot-disk".into(),
            gluon_model::ImageDef {
                name: "boot-disk".into(),
                format: Some(gluon_model::ImageFormat::Fat32),
                size_mb: Some(64),
                entries: vec![
                    ImageEntry {
                        source: ModelImageSource::Esp("default".into()),
                        dest_path: "/".into(),
                    },
                    ImageEntry {
                        source: ModelImageSource::Crate("kernel".into()),
                        dest_path: "/boot/kernel.img".into(),
                    },
                    // host file: must not produce an edge
                    ImageEntry {
                        source: ModelImageSource::File("boot/config.cfg".into()),
                        dest_path: "/boot/config.cfg".into(),
                    },
                ],
                ..Default::default()
            },
        );

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: bl_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: kernel_h,
                    target: th,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        let img_id = *dag
            .node_index
            .get(&DagNode::Image(img_h))
            .expect("Image node must exist");
        let esp_id = *dag.node_index.get(&DagNode::Esp(esp_h)).unwrap();
        let kernel_id = *dag.node_index.get(&dag.crate_node(kernel_h)).unwrap();

        // Esp → Image edge present.
        let esp_outs = dag.edges_out.get(&esp_id).unwrap();
        assert!(
            esp_outs.contains(&img_id),
            "Esp → Image edge must exist (esp must complete before image packs it)"
        );

        // Crate(kernel) → Image edge present.
        let kernel_outs = dag.edges_out.get(&kernel_id).unwrap();
        assert!(
            kernel_outs.contains(&img_id),
            "Crate(kernel) → Image edge must exist for direct crate sources"
        );

        // The image's in-degree counts: 1 from Esp + 1 from Kernel = 2.
        // Bootloader's edge goes to Esp, not directly to Image.
        assert_eq!(*dag.in_degree.get(&img_id).unwrap(), 2);
    }

    // Image with partition.entries and raw_writes(crate=...) sources
    // wires edges to the correct crates. Without this the partitioned
    // path would race the image build ahead of its inputs.
    #[test]
    fn build_dag_image_node_wires_partition_entries_and_raw_writes() {
        use gluon_model::{
            ImageEntry, ImageSource as ModelImageSource, PartitionDef, PartitionTable, RawSource,
            RawWrite,
        };

        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-unknown-none");

        let __group_3 = ensure_group(&mut model, "kernel", Some(th));
        let (kernel_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "crates/kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                group: __group_3,
                ..test_crate_defaults(__group_3)
            },
        );
        let __group_2 = ensure_group(&mut model, "kernel", Some(th));
        let (boot_h, _) = model.crates.insert(
            "boot-sector".into(),
            CrateDef {
                name: "boot-sector".into(),
                path: "crates/boot-sector".into(),
                edition: "2021".into(),
                crate_type: CrateType::RawBinary,
                target: TargetSpec::Cross(th),
                group: __group_2,
                ..test_crate_defaults(__group_2)
            },
        );

        let (img_h, _) = model.images.insert(
            "disk".into(),
            gluon_model::ImageDef {
                name: "disk".into(),
                size_mb: Some(64),
                partition_table: Some(PartitionTable::Mbr),
                raw_writes: vec![RawWrite {
                    lba: 0,
                    source: RawSource::Crate {
                        name: "boot-sector".into(),
                    },
                    max_sectors: Some(1),
                    span: None,
                }],
                partitions: vec![PartitionDef {
                    name: "esp".into(),
                    start_lba: 2048,
                    end_lba: None,
                    fs_type: "fat32".into(),
                    partition_type: 0xEF,
                    bootable: true,
                    entries: vec![ImageEntry {
                        source: ModelImageSource::Crate("kernel".into()),
                        dest_path: "/boot/kernel.bin".into(),
                    }],
                    span: None,
                }],
                ..Default::default()
            },
        );

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: kernel_h,
                    target: th,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: boot_h,
                    target: th,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();
        let img_id = *dag.node_index.get(&DagNode::Image(img_h)).unwrap();
        let kernel_id = *dag.node_index.get(&dag.crate_node(kernel_h)).unwrap();
        let boot_id = *dag.node_index.get(&dag.crate_node(boot_h)).unwrap();

        // partition.entries crate → image edge.
        assert!(
            dag.edges_out.get(&kernel_id).unwrap().contains(&img_id),
            "Crate(kernel) inside partition.entries must edge into the image"
        );
        // raw_writes Crate source → image edge.
        assert!(
            dag.edges_out.get(&boot_id).unwrap().contains(&img_id),
            "Crate(boot-sector) referenced by raw_write must edge into the image"
        );
    }

    // -----------------------------------------------------------------------
    // Test: no_sysroot group skips Sysroot and ConfigCrate edges
    // -----------------------------------------------------------------------
    //
    // A crate whose group has `no_sysroot=true` must:
    //   - NOT have an incoming edge from `Sysroot(its target)`.
    //   - NOT have an incoming edge from `ConfigCrate(its target)`.
    //   - NOT cause `Sysroot(its target)` or `ConfigCrate(its target)`
    //     to appear in the DAG at all (assuming no other crate uses
    //     that target). Otherwise we'd build a sysroot for a target
    //     that can't satisfy `core` (e.g. 16-bit real mode).
    //
    // This is the load-bearing test for lepton-os's BIOS boot sector
    // group. Wire it up wrong and gluon spawns rustc on a doomed
    // sysroot build before the user's first crate gets a chance to
    // compile.
    #[test]
    fn no_sysroot_group_skips_sysroot_and_config_crate_nodes() {
        let mut model = BuildModel::default();

        // Two targets: one normal (kernel), one for boot sectors.
        let kernel_target = make_target(&mut model, "x86_64-unknown-none");
        let bios_target = make_target(&mut model, "i386-code16-boot-sector");

        // Normal kernel group — wants the sysroot.
        let (_kernel_group, _) = model.groups.insert(
            "kernel".into(),
            GroupDef {
                name: "kernel".into(),
                target: TargetSpec::Cross(kernel_target),
                no_sysroot: false,
                ..Default::default()
            },
        );

        // Boot-sector group with no_sysroot=true.
        let (bios_group_h, _) = model.groups.insert(
            "bios-boot".into(),
            GroupDef {
                name: "bios-boot".into(),
                target: TargetSpec::Cross(bios_target),
                no_sysroot: true,
                ..Default::default()
            },
        );

        // Kernel crate (regular).
        let __group_kernel = ensure_group(&mut model, "kernel", Some(kernel_target));
        let kernel_h = model
            .crates
            .insert(
                "kernel".into(),
                CrateDef {
                    name: "kernel".into(),
                    path: "kernel".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::Bin,
                    target: TargetSpec::Cross(kernel_target),
                    group: __group_kernel,
                    ..test_crate_defaults(__group_kernel)
                },
            )
            .0;

        // Boot-sector crate (no_sysroot via group).
        let bs_h = model
            .crates
            .insert(
                "boot-sector".into(),
                CrateDef {
                    name: "boot-sector".into(),
                    path: "boot-sector".into(),
                    edition: "2021".into(),
                    crate_type: CrateType::RawBinary,
                    target: TargetSpec::Cross(bios_target),
                    group: bios_group_h,
                    ..test_crate_defaults(bios_group_h)
                },
            )
            .0;

        let resolved = make_resolved(
            kernel_target,
            vec![
                ResolvedCrateRef {
                    handle: kernel_h,
                    target: kernel_target,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: bs_h,
                    target: bios_target,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        // The bios target must NOT have a Sysroot or ConfigCrate node.
        assert!(
            !dag.node_index.contains_key(&DagNode::Sysroot(bios_target)),
            "no_sysroot group's target must not get a Sysroot node"
        );
        assert!(
            !dag.node_index
                .contains_key(&DagNode::ConfigCrate(bios_target)),
            "no_sysroot group's target must not get a ConfigCrate node"
        );

        // The kernel target should have both (it's a normal cross target).
        assert!(
            dag.node_index
                .contains_key(&DagNode::Sysroot(kernel_target)),
            "regular cross target must still get a Sysroot node"
        );
        assert!(
            dag.node_index
                .contains_key(&DagNode::ConfigCrate(kernel_target)),
            "regular cross target must still get a ConfigCrate node"
        );

        // The boot-sector crate's in-degree must be 0 (no edges from
        // Sysroot or ConfigCrate). The kernel crate's in-degree must
        // include both.
        let bs_id = *dag.node_index.get(&dag.crate_node(bs_h)).unwrap();
        let kernel_id = *dag.node_index.get(&dag.crate_node(kernel_h)).unwrap();
        assert_eq!(
            *dag.in_degree.get(&bs_id).unwrap(),
            0,
            "no_sysroot crate must have no incoming sysroot/config edges"
        );
        assert_eq!(
            *dag.in_degree.get(&kernel_id).unwrap(),
            2,
            "regular cross crate's in-degree = Sysroot + ConfigCrate = 2"
        );
    }

    // -----------------------------------------------------------------------
    // Test: two cross targets produce two ConfigCrate nodes
    // -----------------------------------------------------------------------
    #[test]
    fn build_dag_two_targets_two_config_crates() {
        let mut model = BuildModel::default();
        let t1 = make_target(&mut model, "target-one");
        let t2 = make_target(&mut model, "target-two");

        let c1 = make_cross_crate(&mut model, "crate-one", t1);
        let c2 = make_cross_crate(&mut model, "crate-two", t2);

        let resolved = make_resolved(
            t1,
            vec![
                ResolvedCrateRef {
                    handle: c1,
                    target: t1,
                    host: false,
                },
                ResolvedCrateRef {
                    handle: c2,
                    target: t2,
                    host: false,
                },
            ],
        );

        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).unwrap();

        // Two distinct ConfigCrate nodes.
        let cfg1_id = *dag
            .node_index
            .get(&DagNode::ConfigCrate(t1))
            .expect("ConfigCrate(t1) must exist");
        let cfg2_id = *dag
            .node_index
            .get(&DagNode::ConfigCrate(t2))
            .expect("ConfigCrate(t2) must exist");
        assert_ne!(
            cfg1_id, cfg2_id,
            "different targets → different config nodes"
        );

        // Each cross crate depends on its own ConfigCrate.
        let c1_id = *dag.node_index.get(&dag.crate_node(c1)).unwrap();
        let c2_id = *dag.node_index.get(&dag.crate_node(c2)).unwrap();

        let cfg1_outs = dag.edges_out.get(&cfg1_id).unwrap();
        assert!(cfg1_outs.contains(&c1_id), "ConfigCrate(t1) → crate-one");
        assert!(
            !cfg1_outs.contains(&c2_id),
            "ConfigCrate(t1) must NOT → crate-two"
        );

        let cfg2_outs = dag.edges_out.get(&cfg2_id).unwrap();
        assert!(cfg2_outs.contains(&c2_id), "ConfigCrate(t2) → crate-two");
        assert!(
            !cfg2_outs.contains(&c1_id),
            "ConfigCrate(t2) must NOT → crate-one"
        );
    }

    // -----------------------------------------------------------------------
    // BuildStep DAG ordering tests
    //
    // The BuildStep node must depend on:
    //   1. The runner crate (Crate(step.runner) → BuildStep).
    //   2. Each Crate input listed in `inputs` (Crate(input) → BuildStep).
    // And the BuildStep must be reachable as an artifact source from
    // Image entries that name it.
    // -----------------------------------------------------------------------

    #[test]
    fn build_step_has_runner_and_input_edges() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-cross");

        // Runner is a host bin crate.
        let runner_h = make_host_crate(&mut model, "image-builder");
        // Cross input crate.
        let kernel_h = make_cross_crate(&mut model, "kernel", th);
        // Build step that points at both.
        let mut inputs = BTreeMap::new();
        inputs.insert(
            "KERNEL_PATH".into(),
            BuildStepInput::Crate { handle: kernel_h },
        );
        let (bs_h, _) = model.build_steps.insert(
            "disk-image".into(),
            BuildStepDef {
                name: "disk-image".into(),
                runner: runner_h,
                inputs,
                env: BTreeMap::new(),
                output: "disk.img".into(),
                span: None,
            },
        );

        let resolved = make_resolved(
            th,
            vec![
                ResolvedCrateRef {
                    handle: runner_h,
                    target: th,
                    host: true,
                },
                ResolvedCrateRef {
                    handle: kernel_h,
                    target: th,
                    host: false,
                },
            ],
        );
        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).expect("dag built");

        let bs_id = *dag
            .node_index
            .get(&DagNode::BuildStep(bs_h))
            .expect("BuildStep node must be inserted");
        let runner_id = *dag
            .node_index
            .get(&dag.crate_node(runner_h))
            .expect("runner crate node must exist");
        let kernel_id = *dag
            .node_index
            .get(&dag.crate_node(kernel_h))
            .expect("kernel crate node must exist");

        // Runner → BuildStep edge.
        assert!(
            dag.edges_out
                .get(&runner_id)
                .map(|outs| outs.contains(&bs_id))
                .unwrap_or(false),
            "Crate(runner) → BuildStep edge missing"
        );
        // Input crate → BuildStep edge.
        assert!(
            dag.edges_out
                .get(&kernel_id)
                .map(|outs| outs.contains(&bs_id))
                .unwrap_or(false),
            "Crate(input) → BuildStep edge missing"
        );
    }

    #[test]
    fn image_can_reference_build_step_by_name() {
        // image().write_crate_at(0, "disk-image") should resolve
        // "disk-image" against the build_step arena and add an edge
        // from the BuildStep node to the Image node — proving the
        // unified artifact namespace works.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-cross");

        let runner_h = make_host_crate(&mut model, "image-builder");
        let (bs_h, _) = model.build_steps.insert(
            "disk-image".into(),
            BuildStepDef {
                name: "disk-image".into(),
                runner: runner_h,
                inputs: BTreeMap::new(),
                env: BTreeMap::new(),
                output: "disk.img".into(),
                span: None,
            },
        );
        let (image_h, _) = model.images.insert(
            "boot-disk".into(),
            ImageDef {
                name: "boot-disk".into(),
                raw_writes: vec![gluon_model::RawWrite {
                    lba: 0,
                    max_sectors: None,
                    source: gluon_model::RawSource::Crate {
                        name: "disk-image".into(),
                    },
                    span: None,
                }],
                ..Default::default()
            },
        );

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: runner_h,
                target: th,
                host: true,
            }],
        );
        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).expect("dag built");

        let bs_id = *dag.node_index.get(&DagNode::BuildStep(bs_h)).unwrap();
        let image_id = *dag.node_index.get(&DagNode::Image(image_h)).unwrap();
        assert!(
            dag.edges_out
                .get(&bs_id)
                .map(|outs| outs.contains(&image_id))
                .unwrap_or(false),
            "BuildStep → Image edge missing — unified artifact namespace not wired"
        );
    }

    // -----------------------------------------------------------------------
    // Vendor-pipeline edges (slice 2c-iii)
    // -----------------------------------------------------------------------
    //
    // The `Step 3.5` and consumer-side branches in `build_dag` form the
    // structural contract between the vendor pipeline and the rest of
    // the scheduler. The five tests below pin down each edge category:
    //   1. Cross vendor build → its target's sysroot (depends on it).
    //   2. Host vendor build → no sysroot edge (proc-macros use host
    //      toolchain).
    //   3. Vendor → vendor edges along `VendorDepEdge`s.
    //   4. User crate's `External(_)` dep → `vendor → consumer` edge
    //      when the dep matches a registered vendor build.
    //   5. User crate's `External(_)` dep with no matching vendor entry
    //      → no edge, no panic (V1 host externals + empty-context case).

    use crate::vendor::cross::context::VendorBuildContext;
    use crate::vendor::cross::graph::{VendorCrateBuild, VendorDepEdge};
    use crate::vendor::cross::metadata::{CrateId, CrateKind};
    use gluon_model::{DepSource, ExternalDepDef};
    use std::path::PathBuf;

    fn vendor_build(
        name: &str,
        ver: &str,
        target: TargetSpec,
        kind: CrateKind,
        deps: Vec<VendorDepEdge>,
    ) -> VendorCrateBuild {
        VendorCrateBuild {
            crate_id: CrateId {
                name: name.into(),
                version: ver.into(),
            },
            cargo_target_name: name.replace('-', "_"),
            target,
            kind,
            manifest_dir: PathBuf::from(format!("/vendor/{name}-{ver}")),
            lib_path: PathBuf::from(format!("/vendor/{name}-{ver}/src/lib.rs")),
            edition: "2021".into(),
            features: vec![],
            deps,
        }
    }

    #[test]
    fn vendor_cross_build_gets_sysroot_edge() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let mut vctx = VendorBuildContext::new();
        let vid = vctx
            .add(vendor_build(
                "bitflags",
                "2.11.0",
                TargetSpec::Cross(th),
                CrateKind::Lib,
                vec![],
            ))
            .expect("add vendor build");

        let resolved = make_resolved(th, vec![]);
        let dag = test_dag(&resolved, &model, &vctx).expect("dag built");

        let sysroot_id = *dag.node_index.get(&DagNode::Sysroot(th)).unwrap();
        let v_id = *dag.node_index.get(&dag.vendor_node(vid)).unwrap();
        let sys_outs = dag.edges_out.get(&sysroot_id).expect("sysroot has outs");
        assert!(
            sys_outs.contains(&v_id),
            "cross vendor build must depend on its target's sysroot"
        );
        // Cross vendor build's only predecessor here is the sysroot.
        assert_eq!(*dag.in_degree.get(&v_id).unwrap(), 1);
    }

    #[test]
    fn vendor_host_build_has_no_sysroot_edge() {
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let mut vctx = VendorBuildContext::new();
        let vid = vctx
            .add(vendor_build(
                "syn",
                "2.0.0",
                TargetSpec::Host,
                CrateKind::ProcMacro,
                vec![],
            ))
            .expect("add vendor build");

        let resolved = make_resolved(th, vec![]);
        let dag = test_dag(&resolved, &model, &vctx).expect("dag built");

        let v_id = *dag.node_index.get(&dag.vendor_node(vid)).unwrap();
        // No sysroot edge — host vendor builds compile against host
        // toolchain. In-degree should be 0 (leaf).
        assert_eq!(
            *dag.in_degree.get(&v_id).unwrap(),
            0,
            "host vendor build must be a leaf — no sysroot dep"
        );
    }

    #[test]
    fn vendor_to_vendor_edges_are_wired_along_dep_edges() {
        // `consumer` (cross) depends on `dep` (cross). Verify
        // dep → consumer edge in the DAG.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let mut vctx = VendorBuildContext::new();
        let dep_vid = vctx
            .add(vendor_build(
                "dep-crate",
                "1.0.0",
                TargetSpec::Cross(th),
                CrateKind::Lib,
                vec![],
            ))
            .expect("add dep");
        let consumer_vid = vctx
            .add(vendor_build(
                "consumer-crate",
                "1.0.0",
                TargetSpec::Cross(th),
                CrateKind::Lib,
                vec![VendorDepEdge {
                    extern_name: "dep_crate".into(),
                    crate_id: CrateId {
                        name: "dep-crate".into(),
                        version: "1.0.0".into(),
                    },
                    target: TargetSpec::Cross(th),
                }],
            ))
            .expect("add consumer");

        let resolved = make_resolved(th, vec![]);
        let dag = test_dag(&resolved, &model, &vctx).expect("dag built");

        let dep_id = *dag.node_index.get(&dag.vendor_node(dep_vid)).unwrap();
        let consumer_id = *dag.node_index.get(&dag.vendor_node(consumer_vid)).unwrap();
        let dep_outs = dag.edges_out.get(&dep_id).expect("dep has outs");
        assert!(
            dep_outs.contains(&consumer_id),
            "vendor dep → vendor consumer edge missing"
        );
        // Consumer in-degree: sysroot (1) + dep (1) = 2.
        assert_eq!(*dag.in_degree.get(&consumer_id).unwrap(), 2);
    }

    #[test]
    fn vendor_to_vendor_unresolved_edge_is_hard_error() {
        // VendorCrateBuild.deps points at a (target, name) that the
        // context never registered. This is an internal-consistency
        // failure of the lowering — `build_dag` must surface it loudly.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let mut vctx = VendorBuildContext::new();
        vctx.add(vendor_build(
            "consumer",
            "1.0.0",
            TargetSpec::Cross(th),
            CrateKind::Lib,
            vec![VendorDepEdge {
                extern_name: "ghost".into(),
                crate_id: CrateId {
                    name: "ghost".into(),
                    version: "1.0.0".into(),
                },
                target: TargetSpec::Cross(th),
            }],
        ))
        .expect("add consumer");

        let resolved = make_resolved(th, vec![]);
        let err = test_dag(&resolved, &model, &vctx).expect_err("must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("unresolved dep") && msg.contains("ghost"),
            "error must name the offending edge; got: {msg}"
        );
    }

    #[test]
    fn user_crate_external_dep_resolves_to_vendor_consumer_edge() {
        // User cross crate has an `External(_)` dep whose target+name
        // matches a registered vendor build. Verify
        // `vendor → user crate` edge.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        // Register the vendor build first so we know its id.
        let mut vctx = VendorBuildContext::new();
        let vid = vctx
            .add(vendor_build(
                "bitflags",
                "2.11.0",
                TargetSpec::Cross(th),
                CrateKind::Lib,
                vec![],
            ))
            .expect("add vendor");

        // Register the ExternalDepDef and grab its handle.
        let (ext_h, _) = model.external_deps.insert(
            "bitflags".into(),
            ExternalDepDef {
                name: "bitflags".into(),
                source: DepSource::CratesIo {
                    version: "2.11.0".into(),
                },
                features: vec![],
                default_features: true,
                cfg_flags: vec![],
                rustc_flags: vec![],
                target: TargetSpec::Cross(th),
                span: None,
            },
        );

        // Build the user crate with that External dep keyed by the
        // extern name `bitflags`.
        let group = ensure_group(&mut model, "default", Some(th));
        let (user_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                group,
                deps: {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "bitflags".into(),
                        DepDef {
                            resolution: DepResolution::External(ext_h),
                            features: vec![],
                            version: None,
                            optional: false,
                            default_features: true,
                            span: None,
                        },
                    );
                    m
                },
                ..test_crate_defaults(group)
            },
        );

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: user_h,
                target: th,
                host: false,
            }],
        );
        let dag = test_dag(&resolved, &model, &vctx).expect("dag built");

        let v_id = *dag.node_index.get(&dag.vendor_node(vid)).unwrap();
        let user_id = *dag.node_index.get(&dag.crate_node(user_h)).unwrap();
        let v_outs = dag.edges_out.get(&v_id).expect("vendor has outs");
        assert!(
            v_outs.contains(&user_id),
            "vendor → user-crate edge missing for matching External dep"
        );
        // User crate in-degree: sysroot + config + vendor = 3.
        assert_eq!(*dag.in_degree.get(&user_id).unwrap(), 3);
    }

    #[test]
    fn user_crate_external_dep_with_no_vendor_entry_is_silently_skipped() {
        // V1 host external (or pre-vendor-sync state): the External
        // dep exists on the user crate but the vendor context has no
        // matching entry. Must NOT panic and must NOT add an edge
        // pointing at a non-existent vendor node.
        let mut model = BuildModel::default();
        let th = make_target(&mut model, "x86_64-test");

        let (ext_h, _) = model.external_deps.insert(
            "serde".into(),
            ExternalDepDef {
                name: "serde".into(),
                source: DepSource::CratesIo {
                    version: "1.0.0".into(),
                },
                features: vec![],
                default_features: true,
                cfg_flags: vec![],
                rustc_flags: vec![],
                target: TargetSpec::Host, // V1 path
                span: None,
            },
        );

        let group = ensure_group(&mut model, "default", Some(th));
        let (user_h, _) = model.crates.insert(
            "kernel".into(),
            CrateDef {
                name: "kernel".into(),
                path: "kernel".into(),
                edition: "2021".into(),
                crate_type: CrateType::Bin,
                target: TargetSpec::Cross(th),
                group,
                deps: {
                    let mut m = BTreeMap::new();
                    m.insert(
                        "serde".into(),
                        DepDef {
                            resolution: DepResolution::External(ext_h),
                            features: vec![],
                            version: None,
                            optional: false,
                            default_features: true,
                            span: None,
                        },
                    );
                    m
                },
                ..test_crate_defaults(group)
            },
        );

        let resolved = make_resolved(
            th,
            vec![ResolvedCrateRef {
                handle: user_h,
                target: th,
                host: false,
            }],
        );
        // Empty vendor context — V1 path has no vendor entries.
        let dag = test_dag(&resolved, &model, &VendorBuildContext::new()).expect("dag built");

        // No vendor node should have been created — the only nodes are
        // Sysroot, ConfigCrate, and the user crate.
        let user_id = *dag.node_index.get(&dag.crate_node(user_h)).unwrap();
        // In-degree for the user crate: sysroot + config = 2 (no
        // vendor edge added).
        assert_eq!(*dag.in_degree.get(&user_id).unwrap(), 2);
        assert_eq!(
            dag.len(),
            3,
            "no vendor node should be inserted on lookup miss"
        );
    }
}
