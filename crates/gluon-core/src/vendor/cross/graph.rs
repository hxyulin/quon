//! Lower a [`ResolvedGraph`] into per-crate scheduler-node descriptors.
//!
//! This is the data-only stage between cargo-metadata parsing and the
//! scheduler integration. It walks every non-stub crate in the graph
//! and emits one [`VendorCrateBuild`] per crate, with two invariants
//! enforced as part of the lowering:
//!
//! 1. **Proc-macros pin to host.** Rustc dlopens proc-macros at compile
//!    time, so a proc-macro built for a kernel target would not load.
//!    Both the proc-macro itself and any non-proc-macro dep that the
//!    proc-macro pulls in are placed at [`TargetSpec::Host`] regardless
//!    of the consumer's target. Conversely, *consumers* of a proc-macro
//!    keep their own target — only the edge to the proc-macro flips.
//!
//! 2. **Build scripts are rejected.** Compiling and running a `build.rs`
//!    inside the cross pipeline would require sandboxed execution and
//!    `OUT_DIR`/`cargo:` directive plumbing that the MVP doesn't have.
//!    Any crate with `has_build_script == true` produces a hard error
//!    naming the offender and the consumer chain that pulled it in
//!    (root → … → offender), so the user can see which declared dep
//!    is responsible.
//!
//! All offenders in the graph are collected in one pass and reported
//! together. Reporting them one-at-a-time would force the user into
//! re-run roulette: fix one, find another, repeat.
//!
//! No I/O happens here — [`build_vendor_nodes`] is a pure function over
//! the parsed graph, which keeps it cheap to test against synthetic
//! [`ResolvedGraph`] values and means scheduler integration can layer
//! on top without entangling the two stages.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use gluon_model::TargetSpec;

use super::metadata::{CrateId, CrateKind, ResolvedGraph};

/// One vendored crate's scheduler-node descriptor.
///
/// This mirrors the [`super::metadata::ResolvedCrate`] shape but with
/// gluon-side decisions baked in: target placement (host vs cross) and
/// per-edge target placement. Downstream the scheduler converts each
/// of these into a `BuildNode::VendorCrateBuild` and dispatches a
/// rustc invocation against gluon's per-target sysroot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorCrateBuild {
    pub crate_id: CrateId,
    /// Cargo-normalised name (`-` → `_`) — the form used in
    /// `--extern <name>=<path>`, in artifact filenames, and as the
    /// lookup key in [`gluon_model::ExternalArtifactKey`]. Stored
    /// alongside `crate_id.name` (which keeps the user-facing
    /// hyphenated form) so the scheduler doesn't need to re-derive it.
    pub cargo_target_name: String,
    pub target: TargetSpec,
    pub kind: CrateKind,
    pub manifest_dir: PathBuf,
    pub lib_path: PathBuf,
    pub edition: String,
    pub features: Vec<String>,
    pub deps: Vec<VendorDepEdge>,
}

/// One outgoing dep edge with its post-lowering target. The `target`
/// here is *the target the dep crate is built for*, which differs
/// from the parent's target only when the dep is a proc-macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorDepEdge {
    pub extern_name: String,
    pub crate_id: CrateId,
    pub target: TargetSpec,
}

/// One offender's identity plus the chain of crates that pulled it in.
///
/// `consumer_chain[0]` is always a graph root (a user-declared
/// `external_dep`); `consumer_chain.last()` is always the offender
/// itself. For an offender that *is* a root, the chain has length 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildScriptOffender {
    pub crate_id: CrateId,
    pub consumer_chain: Vec<CrateId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// One or more vendored crates declare a `build.rs`. The MVP cross
    /// pipeline can't run them, so the whole graph is rejected.
    #[error("{}", format_build_script_error(.0))]
    BuildScripts(Vec<BuildScriptOffender>),
}

fn format_build_script_error(offenders: &[BuildScriptOffender]) -> String {
    let mut out = String::from(
        "vendored crate(s) declare a build script (build.rs); gluon's MVP \
         cross-target pipeline does not yet support build scripts. Offenders:\n",
    );
    for off in offenders {
        let chain = off
            .consumer_chain
            .iter()
            .map(|c| format!("{c}"))
            .collect::<Vec<_>>()
            .join(" → ");
        out.push_str(&format!(
            "  - {} (pulled in via: {})\n",
            off.crate_id, chain
        ));
    }
    out
}

/// Lower a [`ResolvedGraph`] into a deterministic list of
/// [`VendorCrateBuild`]s, or fail with the full set of build-script
/// offenders.
///
/// Iteration order is the natural [`BTreeMap`] order of the graph's
/// `CrateId` keys (sorted by `(name, version)`), so two runs with the
/// same input produce identical output. The downstream scheduler does
/// its own topological pass; this function does not need to.
pub fn build_vendor_nodes(graph: &ResolvedGraph) -> Result<Vec<VendorCrateBuild>, GraphError> {
    let offenders: Vec<BuildScriptOffender> = graph
        .crates
        .iter()
        .filter(|(_, c)| c.has_build_script)
        .map(|(id, _)| BuildScriptOffender {
            crate_id: id.clone(),
            consumer_chain: shortest_chain_to(graph, id),
        })
        .collect();
    if !offenders.is_empty() {
        return Err(GraphError::BuildScripts(offenders));
    }

    let mut nodes = Vec::with_capacity(graph.crates.len());
    for (id, c) in &graph.crates {
        let target = target_for(graph, c.kind);
        let deps = c
            .deps
            .iter()
            .map(|d| {
                // Proc-macro deps always pin to host. Non-proc-macro
                // deps inherit the parent's target — which means a
                // proc-macro's lib dep also ends up host (correct: it
                // gets linked into the proc-macro's host shared lib).
                let dep_kind = graph
                    .crates
                    .get(&d.crate_id)
                    .map(|rc| rc.kind)
                    // Defensive: ResolvedGraph is internally
                    // consistent, so missing edges shouldn't happen.
                    // If one slips through, default to Lib so we don't
                    // accidentally pin a missing crate to host.
                    .unwrap_or(CrateKind::Lib);
                VendorDepEdge {
                    extern_name: d.extern_name.clone(),
                    crate_id: d.crate_id.clone(),
                    target: edge_target(target, dep_kind),
                }
            })
            .collect();
        nodes.push(VendorCrateBuild {
            crate_id: id.clone(),
            cargo_target_name: c.cargo_target_name.clone(),
            target,
            kind: c.kind,
            manifest_dir: c.manifest_dir.clone(),
            lib_path: c.lib_path.clone(),
            edition: c.edition.clone(),
            features: c.features.clone(),
            deps,
        });
    }
    Ok(nodes)
}

fn target_for(graph: &ResolvedGraph, kind: CrateKind) -> TargetSpec {
    match kind {
        CrateKind::ProcMacro => TargetSpec::Host,
        CrateKind::Lib => TargetSpec::Cross(graph.target),
    }
}

fn edge_target(parent: TargetSpec, dep_kind: CrateKind) -> TargetSpec {
    match dep_kind {
        CrateKind::ProcMacro => TargetSpec::Host,
        CrateKind::Lib => parent,
    }
}

/// Shortest path from any root to `target`, used in build-script
/// diagnostics.
///
/// Roots are iterated in sorted order so the chosen chain is
/// deterministic on ties. If no root reaches `target` (shouldn't
/// happen for a graph produced by `parse_metadata`), we fall back
/// to a one-element chain so the diagnostic still names the offender.
fn shortest_chain_to(graph: &ResolvedGraph, target: &CrateId) -> Vec<CrateId> {
    if graph.roots.contains(target) {
        return vec![target.clone()];
    }
    let mut roots: Vec<&CrateId> = graph.roots.iter().collect();
    roots.sort();
    let mut best: Option<Vec<CrateId>> = None;
    for root in roots {
        if let Some(path) = bfs_path(graph, root, target) {
            match &best {
                None => best = Some(path),
                Some(b) if path.len() < b.len() => best = Some(path),
                _ => {}
            }
        }
    }
    best.unwrap_or_else(|| vec![target.clone()])
}

fn bfs_path(graph: &ResolvedGraph, root: &CrateId, target: &CrateId) -> Option<Vec<CrateId>> {
    let mut parent: BTreeMap<CrateId, Option<CrateId>> = BTreeMap::new();
    parent.insert(root.clone(), None);
    let mut queue: VecDeque<CrateId> = VecDeque::new();
    queue.push_back(root.clone());
    while let Some(cur) = queue.pop_front() {
        if &cur == target {
            let mut chain = Vec::new();
            let mut node = Some(cur);
            while let Some(n) = node {
                let next = parent.get(&n).cloned().flatten();
                chain.push(n);
                node = next;
            }
            chain.reverse();
            return Some(chain);
        }
        let Some(cn) = graph.crates.get(&cur) else {
            continue;
        };
        for dep in &cn.deps {
            if !parent.contains_key(&dep.crate_id) && graph.crates.contains_key(&dep.crate_id) {
                parent.insert(dep.crate_id.clone(), Some(cur.clone()));
                queue.push_back(dep.crate_id.clone());
            }
        }
    }
    None
}

// -------------------- tests --------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::cross::metadata::{ResolvedCrate, ResolvedDep};
    use gluon_model::{Handle, TargetDef};

    fn target_handle() -> Handle<TargetDef> {
        Handle::<TargetDef>::new(0)
    }

    fn cid(name: &str, ver: &str) -> CrateId {
        CrateId {
            name: name.into(),
            version: ver.into(),
        }
    }

    /// Builder for a `ResolvedCrate` with sensible defaults — keeps the
    /// test fixtures focused on the bits each test cares about.
    fn rc(name: &str, ver: &str, kind: CrateKind, deps: Vec<ResolvedDep>) -> ResolvedCrate {
        ResolvedCrate {
            name: name.into(),
            cargo_target_name: name.replace('-', "_"),
            version: ver.into(),
            manifest_dir: PathBuf::from(format!("/vendor/{name}-{ver}")),
            lib_path: PathBuf::from(format!("/vendor/{name}-{ver}/src/lib.rs")),
            edition: "2021".into(),
            kind,
            features: vec![],
            deps,
            has_build_script: false,
        }
    }

    fn dep(name: &str, ver: &str) -> ResolvedDep {
        ResolvedDep {
            extern_name: name.replace('-', "_"),
            crate_id: cid(name, ver),
        }
    }

    #[test]
    fn lib_root_with_no_deps_pins_to_cross_target() {
        let mut crates = BTreeMap::new();
        crates.insert(
            cid("bitflags", "2.11.0"),
            rc("bitflags", "2.11.0", CrateKind::Lib, vec![]),
        );
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("bitflags", "2.11.0")],
        };

        let nodes = build_vendor_nodes(&graph).expect("ok");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].target, TargetSpec::Cross(target_handle()));
        assert_eq!(nodes[0].kind, CrateKind::Lib);
        assert!(nodes[0].deps.is_empty());
    }

    #[test]
    fn proc_macro_crate_pins_to_host_even_when_consumer_is_cross() {
        let mut crates = BTreeMap::new();
        crates.insert(
            cid("syn", "2.0.0"),
            rc("syn", "2.0.0", CrateKind::ProcMacro, vec![]),
        );
        crates.insert(
            cid("derive_user", "1.0.0"),
            rc(
                "derive_user",
                "1.0.0",
                CrateKind::Lib,
                vec![dep("syn", "2.0.0")],
            ),
        );
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("derive_user", "1.0.0")],
        };

        let nodes = build_vendor_nodes(&graph).expect("ok");
        let syn = nodes.iter().find(|n| n.crate_id.name == "syn").unwrap();
        assert_eq!(syn.target, TargetSpec::Host, "proc-macro must be host");

        let consumer = nodes
            .iter()
            .find(|n| n.crate_id.name == "derive_user")
            .unwrap();
        assert_eq!(
            consumer.target,
            TargetSpec::Cross(target_handle()),
            "consumer keeps cross target"
        );

        let edge = consumer
            .deps
            .iter()
            .find(|e| e.crate_id.name == "syn")
            .expect("edge to syn present");
        assert_eq!(
            edge.target,
            TargetSpec::Host,
            "edge to a proc-macro flips to host"
        );
    }

    #[test]
    fn proc_macros_lib_dep_inherits_host() {
        // syn (proc-macro) → quote (lib). quote ends up host because
        // its consumer is host (proc-macros link their non-proc-macro
        // deps into the host shared library).
        let mut crates = BTreeMap::new();
        crates.insert(
            cid("quote", "1.0.0"),
            rc("quote", "1.0.0", CrateKind::Lib, vec![]),
        );
        crates.insert(
            cid("syn", "2.0.0"),
            rc(
                "syn",
                "2.0.0",
                CrateKind::ProcMacro,
                vec![dep("quote", "1.0.0")],
            ),
        );
        crates.insert(
            cid("user", "1.0.0"),
            rc("user", "1.0.0", CrateKind::Lib, vec![dep("syn", "2.0.0")]),
        );
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("user", "1.0.0")],
        };

        let nodes = build_vendor_nodes(&graph).expect("ok");
        let syn = nodes.iter().find(|n| n.crate_id.name == "syn").unwrap();
        let quote_edge = syn
            .deps
            .iter()
            .find(|e| e.crate_id.name == "quote")
            .unwrap();
        // The edge from syn → quote inherits syn's target (host),
        // because quote is a Lib (not a proc-macro).
        assert_eq!(quote_edge.target, TargetSpec::Host);
    }

    #[test]
    fn build_script_offender_at_root_is_reported_with_chain_of_one() {
        let mut crates = BTreeMap::new();
        let mut bad = rc("bad", "1.0.0", CrateKind::Lib, vec![]);
        bad.has_build_script = true;
        crates.insert(cid("bad", "1.0.0"), bad);
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("bad", "1.0.0")],
        };

        let err = build_vendor_nodes(&graph).expect_err("must reject");
        let GraphError::BuildScripts(offenders) = err;
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].crate_id, cid("bad", "1.0.0"));
        assert_eq!(offenders[0].consumer_chain, vec![cid("bad", "1.0.0")]);
    }

    #[test]
    fn build_script_offender_transitive_reports_full_chain() {
        // root → mid → bad(has_build_script). Chain must be
        // [root, mid, bad].
        let mut crates = BTreeMap::new();
        let mut bad = rc("bad", "1.0.0", CrateKind::Lib, vec![]);
        bad.has_build_script = true;
        crates.insert(cid("bad", "1.0.0"), bad);
        crates.insert(
            cid("mid", "1.0.0"),
            rc("mid", "1.0.0", CrateKind::Lib, vec![dep("bad", "1.0.0")]),
        );
        crates.insert(
            cid("root", "1.0.0"),
            rc("root", "1.0.0", CrateKind::Lib, vec![dep("mid", "1.0.0")]),
        );
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("root", "1.0.0")],
        };

        let err = build_vendor_nodes(&graph).expect_err("must reject");
        let GraphError::BuildScripts(offenders) = err;
        assert_eq!(
            offenders[0].consumer_chain,
            vec![
                cid("root", "1.0.0"),
                cid("mid", "1.0.0"),
                cid("bad", "1.0.0")
            ],
        );
    }

    #[test]
    fn multiple_build_script_offenders_are_collected_in_one_error() {
        let mut crates = BTreeMap::new();
        let mut a = rc("a", "1.0.0", CrateKind::Lib, vec![]);
        a.has_build_script = true;
        let mut b = rc("b", "2.0.0", CrateKind::Lib, vec![]);
        b.has_build_script = true;
        crates.insert(cid("a", "1.0.0"), a);
        crates.insert(cid("b", "2.0.0"), b);
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("a", "1.0.0"), cid("b", "2.0.0")],
        };

        let err = build_vendor_nodes(&graph).expect_err("must reject");
        let GraphError::BuildScripts(offenders) = err;
        assert_eq!(offenders.len(), 2);
        // BTreeMap iteration is sorted by CrateId, so a@1.0.0 before b@2.0.0.
        assert_eq!(offenders[0].crate_id, cid("a", "1.0.0"));
        assert_eq!(offenders[1].crate_id, cid("b", "2.0.0"));
    }

    #[test]
    fn proc_macro_with_build_script_is_still_rejected() {
        // ProcMacro kind doesn't excuse a build script — both are
        // independent failure modes, and only the build-script bit
        // gates lowering.
        let mut crates = BTreeMap::new();
        let mut pm = rc("pm", "1.0.0", CrateKind::ProcMacro, vec![]);
        pm.has_build_script = true;
        crates.insert(cid("pm", "1.0.0"), pm);
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("pm", "1.0.0")],
        };

        let err = build_vendor_nodes(&graph).expect_err("must reject");
        let GraphError::BuildScripts(o) = err;
        assert_eq!(o.len(), 1);
    }

    #[test]
    fn output_order_is_deterministic_across_runs() {
        let mut crates = BTreeMap::new();
        crates.insert(cid("z", "1.0.0"), rc("z", "1.0.0", CrateKind::Lib, vec![]));
        crates.insert(cid("a", "1.0.0"), rc("a", "1.0.0", CrateKind::Lib, vec![]));
        crates.insert(cid("m", "1.0.0"), rc("m", "1.0.0", CrateKind::Lib, vec![]));
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![],
        };

        let n1 = build_vendor_nodes(&graph).expect("ok");
        let n2 = build_vendor_nodes(&graph).expect("ok");
        assert_eq!(n1, n2);
        let names: Vec<&str> = n1.iter().map(|n| n.crate_id.name.as_str()).collect();
        assert_eq!(names, vec!["a", "m", "z"], "BTreeMap order");
    }

    #[test]
    fn dep_edges_propagate_features_independently() {
        // Sanity check that features on the parent crate are passed
        // through unchanged — they're load-bearing for the rustc
        // command builder in the next slice.
        let mut bf = rc("bitflags", "2.11.0", CrateKind::Lib, vec![]);
        bf.features = vec!["alloc".into(), "std".into()];
        let mut crates = BTreeMap::new();
        crates.insert(cid("bitflags", "2.11.0"), bf);
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("bitflags", "2.11.0")],
        };

        let nodes = build_vendor_nodes(&graph).expect("ok");
        assert_eq!(
            nodes[0].features,
            vec!["alloc".to_string(), "std".to_string()]
        );
    }
}
