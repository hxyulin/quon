//! Lower a [`ResolvedGraph`] into per-unit scheduler-node descriptors.
//!
//! This is the data-only stage between cargo-metadata parsing and the
//! scheduler integration. It decides which platform each crate is built
//! for and emits one [`VendorCrateBuild`] per `(crate, platform)` unit.
//!
//! # Placement by reachability
//!
//! Rustc loads proc macros with `dlopen`, so a proc macro and every
//! crate linked into it must be built for the host. Placement therefore
//! follows the dependency edges instead of the crate kind:
//!
//! - The **target closure** starts at the graph roots (the user's
//!   declared deps) and follows normal edges, stopping at proc macros.
//!   Each crate in it gets a [`TargetSpec::Cross`] unit.
//! - The **host closure** holds every proc macro reached from either
//!   closure, every build-dependency of a placed crate with a build
//!   script, and every normal dep of a host unit. Each crate in it gets
//!   a [`TargetSpec::Host`] unit.
//!
//! A crate in both closures (for example `memchr` used by a kernel crate
//! and by a proc macro) gets two units. Crates reached by neither, such
//! as those only behind dev edges, get none.
//!
//! An edge from a unit to a proc macro points at the host unit. Every
//! other edge stays on the parent's platform.
//!
//! # One feature set for both platforms
//!
//! `cargo metadata` reports one node per package, so both units of a
//! dual-platform crate carry the same feature set. Cargo's resolver 2
//! would unify host and target features separately. Until gluon has its
//! own resolution, a crate whose host and target features must differ
//! (`serde` with `std` on host only) gets the union on both platforms,
//! which can break a `no_std` target build.
//!
//! # Build scripts
//!
//! Each unit of a crate with a build script carries a
//! [`VendorBuildScript`]: the script, its build-dep edges (always to
//! host units), and the package metadata. `crate::unit::lower` turns
//! it into a script unit and one run per owning unit.
//!
//! No I/O happens here: [`build_vendor_nodes`] is a pure function over
//! the parsed graph, which keeps it cheap to test against synthetic
//! [`ResolvedGraph`] values.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use gluon_model::TargetSpec;

use super::metadata::{CrateId, CrateKind, ResolvedGraph};
use crate::build_script::PackageInfo;

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
    /// Set when the package has a build script that runs for this unit.
    pub build_script: Option<VendorBuildScript>,
}

/// The build script of a [`VendorCrateBuild`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorBuildScript {
    pub path: PathBuf,
    pub edition: String,
    /// Build-dep edges. Every target is [`TargetSpec::Host`].
    pub build_deps: Vec<VendorDepEdge>,
    pub package: PackageInfo,
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

/// Lower a [`ResolvedGraph`] into a deterministic list of
/// [`VendorCrateBuild`]s.
///
/// Output order is the [`BTreeMap`] order of the graph's `CrateId` keys
/// (sorted by `(name, version)`), with a crate's cross unit before its
/// host unit, so two runs with the same input produce identical output.
/// The scheduler does its own topological pass.
pub fn build_vendor_nodes(graph: &ResolvedGraph) -> Vec<VendorCrateBuild> {
    let placement = place_units(graph);
    let host_edge = |d: &super::metadata::ResolvedDep| VendorDepEdge {
        extern_name: d.extern_name.clone(),
        crate_id: d.crate_id.clone(),
        target: TargetSpec::Host,
    };

    let mut nodes = Vec::with_capacity(graph.crates.len());
    for (id, c) in &graph.crates {
        let Some(platforms) = placement.get(id) else {
            continue;
        };
        let cross = TargetSpec::Cross(graph.target);
        let targets = [(platforms.cross, cross), (platforms.host, TargetSpec::Host)];
        for target in targets.into_iter().filter(|(on, _)| *on).map(|(_, t)| t) {
            let deps = c
                .deps
                .iter()
                .map(|d| VendorDepEdge {
                    extern_name: d.extern_name.clone(),
                    crate_id: d.crate_id.clone(),
                    target: edge_target(target, kind_of(graph, &d.crate_id)),
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
                build_script: c.build_script.as_ref().map(|b| VendorBuildScript {
                    path: b.path.clone(),
                    edition: b.edition.clone(),
                    build_deps: b.build_deps.iter().map(host_edge).collect(),
                    package: b.package.clone(),
                }),
            });
        }
    }
    nodes
}

/// Which platforms a crate needs a unit on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Platforms {
    cross: bool,
    host: bool,
}

/// Compute the target and host closures described in the module docs.
///
/// One worklist walk over `(crate, platform)` pairs: each pair is
/// visited once, and its deps are queued on the platform
/// [`edge_target`] picks. Crates missing from the map are reached by
/// neither closure and get no unit.
fn place_units(graph: &ResolvedGraph) -> BTreeMap<CrateId, Platforms> {
    let mut placed: BTreeMap<CrateId, Platforms> = BTreeMap::new();
    let mut queue: VecDeque<(CrateId, TargetSpec)> = VecDeque::new();
    let cross = TargetSpec::Cross(graph.target);
    for root in &graph.roots {
        queue.push_back((root.clone(), edge_target(cross, kind_of(graph, root))));
    }
    while let Some((id, target)) = queue.pop_front() {
        let Some(c) = graph.crates.get(&id) else {
            continue;
        };
        let entry = placed.entry(id).or_default();
        let slot = match target {
            TargetSpec::Host => &mut entry.host,
            TargetSpec::Cross(_) => &mut entry.cross,
        };
        if *slot {
            continue;
        }
        *slot = true;
        for d in &c.deps {
            let dep_target = edge_target(target, kind_of(graph, &d.crate_id));
            queue.push_back((d.crate_id.clone(), dep_target));
        }
        // Build scripts run on the host, so their deps are host units
        // whatever platform the script's package is built for.
        for d in c.build_script.iter().flat_map(|b| &b.build_deps) {
            queue.push_back((d.crate_id.clone(), TargetSpec::Host));
        }
    }
    placed
}

/// Kind of a crate in the graph. A missing crate defaults to `Lib` so
/// a dangling edge is never silently moved to the host; the DAG builder
/// reports it as an unresolved dep instead.
fn kind_of(graph: &ResolvedGraph, id: &CrateId) -> CrateKind {
    graph
        .crates
        .get(id)
        .map(|c| c.kind)
        .unwrap_or(CrateKind::Lib)
}

fn edge_target(parent: TargetSpec, dep_kind: CrateKind) -> TargetSpec {
    match dep_kind {
        CrateKind::ProcMacro => TargetSpec::Host,
        CrateKind::Lib => parent,
    }
}

// -------------------- tests --------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vendor::cross::metadata::{ResolvedBuildScript, ResolvedCrate, ResolvedDep};
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
            build_script: None,
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

        let nodes = build_vendor_nodes(&graph);
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

        let nodes = build_vendor_nodes(&graph);
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

        let nodes = build_vendor_nodes(&graph);
        let syn = nodes.iter().find(|n| n.crate_id.name == "syn").unwrap();
        let quote_edge = syn
            .deps
            .iter()
            .find(|e| e.crate_id.name == "quote")
            .unwrap();
        // The edge from syn → quote inherits syn's target (host),
        // because quote is a Lib (not a proc-macro).
        assert_eq!(quote_edge.target, TargetSpec::Host);

        // The edge must land on a real unit: quote is reachable only
        // through the proc macro, so it has one host unit and no cross
        // unit.
        assert_eq!(
            units(&nodes, "quote"),
            vec![TargetSpec::Host],
            "quote must be host only"
        );
    }

    /// Platforms of every unit of `name`, in output order.
    fn units(nodes: &[VendorCrateBuild], name: &str) -> Vec<TargetSpec> {
        nodes
            .iter()
            .filter(|n| n.crate_id.name == name)
            .map(|n| n.target)
            .collect()
    }

    #[test]
    fn transitive_deps_of_a_proc_macro_are_host_only() {
        // kernel → derive (proc macro) → syn → proc-macro2 → unicode-ident.
        // Only `kernel` belongs to the target; the whole syn stack is
        // host, at every depth.
        let mut crates = BTreeMap::new();
        crates.insert(
            cid("unicode-ident", "1.0.0"),
            rc("unicode-ident", "1.0.0", CrateKind::Lib, vec![]),
        );
        crates.insert(
            cid("proc-macro2", "1.0.0"),
            rc(
                "proc-macro2",
                "1.0.0",
                CrateKind::Lib,
                vec![dep("unicode-ident", "1.0.0")],
            ),
        );
        crates.insert(
            cid("syn", "2.0.0"),
            rc(
                "syn",
                "2.0.0",
                CrateKind::Lib,
                vec![dep("proc-macro2", "1.0.0")],
            ),
        );
        crates.insert(
            cid("derive", "1.0.0"),
            rc(
                "derive",
                "1.0.0",
                CrateKind::ProcMacro,
                vec![dep("syn", "2.0.0")],
            ),
        );
        crates.insert(
            cid("kernel", "1.0.0"),
            rc(
                "kernel",
                "1.0.0",
                CrateKind::Lib,
                vec![dep("derive", "1.0.0")],
            ),
        );
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("kernel", "1.0.0")],
        };

        let nodes = build_vendor_nodes(&graph);
        let cross = TargetSpec::Cross(target_handle());
        assert_eq!(units(&nodes, "kernel"), vec![cross]);
        for name in ["derive", "syn", "proc-macro2", "unicode-ident"] {
            assert_eq!(units(&nodes, name), vec![TargetSpec::Host], "{name}");
        }
        // Every host unit's edges stay on the host.
        for n in nodes.iter().filter(|n| n.target == TargetSpec::Host) {
            assert!(n.deps.iter().all(|e| e.target == TargetSpec::Host));
        }
    }

    #[test]
    fn lib_reachable_from_target_and_proc_macro_gets_two_units() {
        // kernel → memchr, kernel → derive (proc macro) → memchr.
        // memchr is linked into the kernel and into the proc macro, so
        // it needs a cross unit and a host unit with the same features
        // (cargo metadata reports one feature set per package).
        let mut memchr = rc("memchr", "2.7.0", CrateKind::Lib, vec![]);
        memchr.features = vec!["alloc".into()];
        let mut crates = BTreeMap::new();
        crates.insert(cid("memchr", "2.7.0"), memchr);
        crates.insert(
            cid("derive", "1.0.0"),
            rc(
                "derive",
                "1.0.0",
                CrateKind::ProcMacro,
                vec![dep("memchr", "2.7.0")],
            ),
        );
        crates.insert(
            cid("kernel", "1.0.0"),
            rc(
                "kernel",
                "1.0.0",
                CrateKind::Lib,
                vec![dep("derive", "1.0.0"), dep("memchr", "2.7.0")],
            ),
        );
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("kernel", "1.0.0")],
        };

        let nodes = build_vendor_nodes(&graph);
        let cross = TargetSpec::Cross(target_handle());
        assert_eq!(units(&nodes, "memchr"), vec![cross, TargetSpec::Host]);
        for n in nodes.iter().filter(|n| n.crate_id.name == "memchr") {
            assert_eq!(n.features, vec!["alloc".to_string()]);
        }

        // Each edge resolves to the unit on the expected platform, so
        // both units register in the context without a duplicate key.
        let kernel = nodes.iter().find(|n| n.crate_id.name == "kernel").unwrap();
        let edge = |name: &str| {
            kernel
                .deps
                .iter()
                .find(|e| e.crate_id.name == name)
                .unwrap()
                .target
        };
        assert_eq!(edge("memchr"), cross);
        assert_eq!(edge("derive"), TargetSpec::Host);
        let derive = nodes.iter().find(|n| n.crate_id.name == "derive").unwrap();
        assert_eq!(derive.deps[0].target, TargetSpec::Host);

        let mut ctx = super::super::context::VendorBuildContext::new();
        for n in nodes {
            ctx.add(n).expect("no duplicate (target, name) key");
        }
    }

    #[test]
    fn proc_macro_root_is_host_only() {
        let mut crates = BTreeMap::new();
        crates.insert(
            cid("derive", "1.0.0"),
            rc("derive", "1.0.0", CrateKind::ProcMacro, vec![]),
        );
        let graph = ResolvedGraph {
            target: target_handle(),
            crates,
            roots: vec![cid("derive", "1.0.0")],
        };
        let nodes = build_vendor_nodes(&graph);
        assert_eq!(units(&nodes, "derive"), vec![TargetSpec::Host]);
    }

    /// Give `c` a build script with `build_deps`.
    fn with_script(mut c: ResolvedCrate, build_deps: Vec<ResolvedDep>) -> ResolvedCrate {
        c.build_script = Some(ResolvedBuildScript {
            path: c.manifest_dir.join("build.rs"),
            edition: "2021".into(),
            build_deps,
            package: PackageInfo {
                name: c.name.clone(),
                version: c.version.clone(),
                links: None,
                manifest_dir: c.manifest_dir.clone(),
                manifest_path: c.manifest_dir.join("Cargo.toml"),
                ..PackageInfo::default()
            },
        });
        c
    }

    fn graph_of(crates: Vec<ResolvedCrate>, roots: &[&str]) -> ResolvedGraph {
        let crates: BTreeMap<CrateId, ResolvedCrate> = crates
            .into_iter()
            .map(|c| (cid(&c.name, &c.version), c))
            .collect();
        let roots = roots
            .iter()
            .map(|r| {
                crates
                    .keys()
                    .find(|k| k.name == *r)
                    .expect("root is in the graph")
                    .clone()
            })
            .collect();
        ResolvedGraph {
            target: target_handle(),
            crates,
            roots,
        }
    }

    #[test]
    fn crates_reached_by_no_edge_get_no_unit() {
        // `cc` has a build script but nothing reaches it.
        let graph = graph_of(
            vec![
                with_script(rc("cc", "1.0.0", CrateKind::Lib, vec![]), vec![]),
                rc("bitflags", "2.11.0", CrateKind::Lib, vec![]),
            ],
            &["bitflags"],
        );
        let nodes = build_vendor_nodes(&graph);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].crate_id.name, "bitflags");
    }

    #[test]
    fn crate_with_a_build_script_carries_it_on_each_unit() {
        // kernel → zc (script), kernel → derive (proc macro) → zc. Both
        // units of zc run the script.
        let graph = graph_of(
            vec![
                with_script(rc("zc", "0.8.0", CrateKind::Lib, vec![]), vec![]),
                rc(
                    "derive",
                    "1.0.0",
                    CrateKind::ProcMacro,
                    vec![dep("zc", "0.8.0")],
                ),
                rc(
                    "kernel",
                    "1.0.0",
                    CrateKind::Lib,
                    vec![dep("derive", "1.0.0"), dep("zc", "0.8.0")],
                ),
            ],
            &["kernel"],
        );
        let nodes = build_vendor_nodes(&graph);
        let zc: Vec<&VendorCrateBuild> = nodes.iter().filter(|n| n.crate_id.name == "zc").collect();
        assert_eq!(zc.len(), 2);
        for n in zc {
            let script = n.build_script.as_ref().expect("script kept");
            assert_eq!(script.path, PathBuf::from("/vendor/zc-0.8.0/build.rs"));
            assert_eq!(script.package.name, "zc");
        }
        let kernel = nodes.iter().find(|n| n.crate_id.name == "kernel").unwrap();
        assert!(kernel.build_script.is_none());
    }

    #[test]
    fn build_deps_and_their_deps_are_host_only() {
        // kernel-sys (cross, script) has build-dep cc → shlex. Only the
        // script uses them, so they are host units, and the edges from
        // the script point at the host.
        let graph = graph_of(
            vec![
                rc("shlex", "1.0.0", CrateKind::Lib, vec![]),
                rc("cc", "1.0.0", CrateKind::Lib, vec![dep("shlex", "1.0.0")]),
                with_script(
                    rc("kernel-sys", "0.1.0", CrateKind::Lib, vec![]),
                    vec![dep("cc", "1.0.0")],
                ),
            ],
            &["kernel-sys"],
        );
        let nodes = build_vendor_nodes(&graph);
        let cross = TargetSpec::Cross(target_handle());
        assert_eq!(units(&nodes, "kernel-sys"), vec![cross]);
        assert_eq!(units(&nodes, "cc"), vec![TargetSpec::Host]);
        assert_eq!(units(&nodes, "shlex"), vec![TargetSpec::Host]);
        let sys = nodes
            .iter()
            .find(|n| n.crate_id.name == "kernel-sys")
            .unwrap();
        let script = sys.build_script.as_ref().unwrap();
        assert_eq!(
            script.build_deps,
            vec![VendorDepEdge {
                extern_name: "cc".into(),
                crate_id: cid("cc", "1.0.0"),
                target: TargetSpec::Host,
            }]
        );
        assert!(
            sys.deps.is_empty(),
            "a build-dep is not linked into the crate"
        );
    }

    #[test]
    fn crate_that_is_a_build_dep_and_a_normal_dep_gets_two_units() {
        // memchr is linked into the kernel and used by a build script.
        let graph = graph_of(
            vec![
                rc("memchr", "2.7.0", CrateKind::Lib, vec![]),
                with_script(
                    rc(
                        "kernel",
                        "1.0.0",
                        CrateKind::Lib,
                        vec![dep("memchr", "2.7.0")],
                    ),
                    vec![dep("memchr", "2.7.0")],
                ),
            ],
            &["kernel"],
        );
        let nodes = build_vendor_nodes(&graph);
        let cross = TargetSpec::Cross(target_handle());
        assert_eq!(units(&nodes, "memchr"), vec![cross, TargetSpec::Host]);
        let mut ctx = super::super::context::VendorBuildContext::new();
        for n in nodes {
            ctx.add(n).expect("no duplicate (target, name) key");
        }
    }

    #[test]
    fn proc_macro_with_a_build_script_is_a_host_unit_with_its_script() {
        let graph = graph_of(
            vec![
                with_script(rc("pm", "1.0.0", CrateKind::ProcMacro, vec![]), vec![]),
                rc("user", "1.0.0", CrateKind::Lib, vec![dep("pm", "1.0.0")]),
            ],
            &["user"],
        );
        let nodes = build_vendor_nodes(&graph);
        assert_eq!(units(&nodes, "pm"), vec![TargetSpec::Host]);
        let pm = nodes.iter().find(|n| n.crate_id.name == "pm").unwrap();
        assert!(pm.build_script.is_some());
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
            roots: vec![cid("z", "1.0.0"), cid("a", "1.0.0"), cid("m", "1.0.0")],
        };

        let n1 = build_vendor_nodes(&graph);
        let n2 = build_vendor_nodes(&graph);
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

        let nodes = build_vendor_nodes(&graph);
        assert_eq!(
            nodes[0].features,
            vec!["alloc".to_string(), "std".to_string()]
        );
    }
}
