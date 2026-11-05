//! Parse `cargo metadata` output into a target-agnostic resolved graph.
//!
//! After `cargo vendor` populates the per-target scratch workspace
//! (one per cross target), we shell out to:
//!
//! ```text
//! cargo metadata --frozen --format-version 1 \
//!     --manifest-path build/vendor-workspaces/<target>/Cargo.toml
//! ```
//!
//! and parse the resulting JSON into a [`ResolvedGraph`] — a typed
//! snapshot of the resolved package set, the lib/proc-macro targets,
//! and the per-package dependency edges plus active features. The
//! graph is the single source of truth that the next stage (graph →
//! scheduler nodes) consumes; nothing downstream looks at cargo's raw
//! JSON.
//!
//! # Why parse it ourselves rather than pull in `cargo_metadata`
//!
//! We use only a small subset of cargo's metadata schema (packages,
//! targets, the resolve node graph). Hand-rolling with `serde_json`
//! avoids pulling in a transitive `semver`/`camino`/etc. surface and
//! lets us pin the schema we depend on. If cargo's JSON schema
//! changes incompatibly in a future cargo release, the failure is
//! local to this module.
//!
//! # What we deliberately drop
//!
//! - The synthetic workspace stub (`gluon-vendor-workspace`) — it has
//!   no consumer; everything pulls deps directly from the user's
//!   declared `external_dep` set.
//! - `dev-dependencies` — vendored deps don't run their tests through
//!   gluon, so dev-only edges would just bloat the graph and pull in
//!   crates we never compile.
//! - Target-conditional deps that don't apply to the platform they are
//!   built for: a normal edge's `dep_kinds[].target` key is matched
//!   against the cross target's probed cfg set, a build edge's against
//!   the host's, since build scripts run on the host. (See
//!   [`is_dep_active`].)
//!
//! # Build scripts
//!
//! A package with a `custom-build` target gets a
//! [`ResolvedBuildScript`]: the script path, its build-dependency edges
//! (`dep_kinds[].kind == "build"`), and the package metadata Cargo
//! passes to the script as `CARGO_PKG_*`.
//!
//! # Version handling
//!
//! We store the version as a plain `String` rather than `semver::Version`
//! because nothing downstream compares or sorts versions — the
//! `(name, version)` `CrateId` is only an opaque key into the graph.
//! If a future feature (e.g. cross-version conflict diagnostics) needs
//! semver semantics, swap the field type in [`CrateId`] and the
//! string-parse points; everything else round-trips unchanged.

use crate::build_script::PackageInfo;
use crate::manifest::platform::{TargetPlatform, parse_platform};
use gluon_model::{Handle, TargetDef};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Internal-only stub package name we filter out. Matches the constant
/// in [`super::super::manifest_gen`]; duplicating it keeps the metadata
/// module independently testable without dragging the manifest_gen
/// module into its compile graph.
const WORKSPACE_STUB_NAME: &str = "gluon-vendor-workspace";

/// Errors surfaced by [`parse_metadata`]. Each variant carries enough
/// context for the caller to render a precise diagnostic — the metadata
/// module deliberately doesn't depend on the project-wide `Diagnostic`
/// type so it can be reused outside the build path (e.g. from tests).
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("cargo metadata JSON failed to deserialize: {0}")]
    Deserialize(#[from] serde_json::Error),

    #[error("package {pkg_id} is referenced by resolve.nodes but is missing from packages[]")]
    MissingPackage { pkg_id: String },

    #[error(
        "package {name}@{version} declares no library target (kind \"lib\" or \"proc-macro\"); \
         only library deps can be vendored cross-target"
    )]
    NoLibTarget { name: String, version: String },

    #[error(
        "package {name}@{version} declares both a lib and a proc-macro target — \
         this is unsupported; pick one"
    )]
    AmbiguousLibKind { name: String, version: String },

    #[error("cargo metadata output is missing a `resolve` section; was --frozen used incorrectly?")]
    MissingResolve,

    #[error("dependency `{dep}` has an unparseable target key `{key}`")]
    InvalidPlatform { dep: String, key: String },
}

pub type Result<T> = std::result::Result<T, ParseError>;

// -------------------- public types --------------------

/// Fully-resolved dependency graph for one target's vendored set.
///
/// `target` is the gluon target this graph was resolved for — it's the
/// concrete `TargetSpec::Cross(Handle<TargetDef>)` (the host pipeline
/// uses cargo as the builder and never produces a `ResolvedGraph`).
/// `crates` is keyed by `(name, version)` so the same crate at two
/// versions (rare but possible with transitives) stays distinguishable.
/// `roots` lists the crates the user declared via
/// `cfg.dependency(...).target(t)` — the entry points the scheduler
/// must build to satisfy the consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGraph {
    pub target: Handle<TargetDef>,
    pub crates: BTreeMap<CrateId, ResolvedCrate>,
    pub roots: Vec<CrateId>,
}

/// Stable identity of a resolved crate within a [`ResolvedGraph`].
///
/// Cargo's PackageId strings are deliberately *not* used here —
/// they include source URLs and have evolved across cargo versions.
/// `(name, version)` is sufficient for our purposes: each scratch
/// workspace's resolution is internally consistent (cargo would have
/// rejected a duplicate `(name, version)` already), and we never
/// cross-reference two graphs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CrateId {
    pub name: String,
    pub version: String,
}

impl std::fmt::Display for CrateId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// One vendored crate's resolved metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCrate {
    /// User-declared name (may contain hyphens, e.g. `proc-macro2`).
    pub name: String,
    /// Cargo-normalised target name (`-` → `_`). This is the name that
    /// shows up as `--extern <name>=<path>` and as the rlib filename
    /// stem — the compile path keys [`gluon_model::ExternalArtifactKey`]
    /// on this form.
    pub cargo_target_name: String,
    pub version: String,
    /// Directory containing this crate's `Cargo.toml`. Lives under
    /// `<project>/vendor/<crate>-<version>/` once `cargo vendor` has
    /// populated the tree.
    pub manifest_dir: PathBuf,
    /// Absolute path to the lib root (`src/lib.rs` or whatever the
    /// crate's lib target's `src_path` is).
    pub lib_path: PathBuf,
    /// Rust edition declared by the lib target. Falls back to the
    /// package edition if the target doesn't specify one.
    pub edition: String,
    pub kind: CrateKind,
    /// Features active for this crate in this resolution. Sorted for
    /// determinism (cargo emits them sorted; we re-sort defensively).
    pub features: Vec<String>,
    /// Direct dependencies edges, sorted by `extern_name` for
    /// deterministic downstream iteration.
    pub deps: Vec<ResolvedDep>,
    /// Set when the package has a `custom-build` target.
    pub build_script: Option<ResolvedBuildScript>,
}

/// A package's build script, as `cargo metadata` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBuildScript {
    /// The script's `src_path` (`build.rs`, or `package.build`).
    pub path: PathBuf,
    /// Edition of the `custom-build` target.
    pub edition: String,
    /// Build-dependency edges, sorted by `extern_name`. They are
    /// compiled for the host.
    pub build_deps: Vec<ResolvedDep>,
    /// `CARGO_PKG_*`, `CARGO_MANIFEST_*` and `links` for the script.
    pub package: PackageInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrateKind {
    Lib,
    ProcMacro,
}

/// One outgoing dep edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDep {
    /// Name as the consumer sees it via `extern crate foo` / `use foo`.
    /// Accounts for cargo's `package = "..."` rename trick.
    pub extern_name: String,
    pub crate_id: CrateId,
}

// -------------------- raw schema (private) --------------------

/// Subset of cargo's `--format-version 1` JSON we actually need.
/// Field naming mirrors cargo's output exactly so `serde_json` can
/// deserialize without rename attributes.
#[derive(Debug, Deserialize)]
struct RawMetadata {
    packages: Vec<RawPackage>,
    workspace_members: Vec<String>,
    resolve: Option<RawResolve>,
}

#[derive(Debug, Deserialize)]
struct RawPackage {
    id: String,
    name: String,
    version: String,
    edition: String,
    manifest_path: PathBuf,
    targets: Vec<RawTarget>,
    // Package metadata for a build script's `CARGO_PKG_*`.
    #[serde(default)]
    links: Option<String>,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    license_file: Option<String>,
    #[serde(default)]
    rust_version: Option<String>,
    #[serde(default)]
    readme: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTarget {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
    /// Per-target edition override; absent → use package edition.
    #[serde(default)]
    edition: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawResolve {
    nodes: Vec<RawNode>,
}

#[derive(Debug, Deserialize)]
struct RawNode {
    id: String,
    deps: Vec<RawNodeDep>,
    #[serde(default)]
    features: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawNodeDep {
    /// Extern name as the consumer sees it (post-rename).
    name: String,
    /// PackageId pointing at the resolved package in `packages[]`.
    pkg: String,
    /// Per-edge kind/target metadata. We use this to drop dev/build
    /// edges and to filter target-conditional deps.
    #[serde(default)]
    dep_kinds: Vec<RawDepKind>,
}

#[derive(Debug, Deserialize)]
struct RawDepKind {
    /// `null` for normal, `"dev"` for dev-deps, `"build"` for build-deps.
    #[serde(default)]
    kind: Option<String>,
    /// `null` for unconditional, otherwise a cfg expression like
    /// `cfg(unix)` or a target triple, matched against the target's
    /// probed cfg set.
    #[serde(default)]
    target: Option<String>,
}

// -------------------- public entry point --------------------

/// Parse cargo metadata JSON for `target` into a [`ResolvedGraph`].
///
/// `target` is the gluon target handle the resulting graph belongs to —
/// the JSON itself doesn't know about gluon's target model, so the
/// caller must pass it in. The handle is stored on the graph and used
/// downstream when emitting scheduler nodes and artifact keys.
/// `platform` is the target's probed cfg set, used to keep the
/// target-conditional normal edges that apply. `host` is the host's,
/// used for build edges.
pub fn parse_metadata(
    json: &str,
    target: Handle<TargetDef>,
    platform: &TargetPlatform,
    host: &TargetPlatform,
) -> Result<ResolvedGraph> {
    let raw: RawMetadata = serde_json::from_str(json)?;
    let resolve = raw.resolve.ok_or(ParseError::MissingResolve)?;

    // Build a PackageId → RawPackage map up front so resolve.nodes can
    // look up name/version/targets in O(1) without re-scanning packages
    // for every edge.
    let by_id: BTreeMap<&str, &RawPackage> =
        raw.packages.iter().map(|p| (p.id.as_str(), p)).collect();

    // Identify the workspace stub package: it is the one (and only)
    // workspace member whose name matches our manifest_gen stub. We
    // drop it from the output graph entirely (no consumer wants it),
    // and we use *its* resolve node's deps as `roots` (those are the
    // user-declared external_deps).
    let stub_id = workspace_stub_id(&raw.workspace_members, &by_id);

    let mut crates: BTreeMap<CrateId, ResolvedCrate> = BTreeMap::new();
    let mut roots: Vec<CrateId> = Vec::new();

    // Walk every node in the resolve graph and lower it into a
    // ResolvedCrate. The stub node is skipped from `crates` but its
    // deps become `roots`.
    for node in &resolve.nodes {
        if Some(node.id.as_str()) == stub_id {
            // Roots = direct deps of the synthetic workspace package.
            for dep in &node.deps {
                if !is_dep_active(dep, None, platform)? {
                    continue;
                }
                let pkg = by_id.get(dep.pkg.as_str()).copied().ok_or_else(|| {
                    ParseError::MissingPackage {
                        pkg_id: dep.pkg.clone(),
                    }
                })?;
                roots.push(CrateId {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                });
            }
            continue;
        }

        let pkg =
            by_id
                .get(node.id.as_str())
                .copied()
                .ok_or_else(|| ParseError::MissingPackage {
                    pkg_id: node.id.clone(),
                })?;

        // Skip the stub even if it slipped through workspace_members
        // detection (defensive — different cargo versions order
        // workspace_members differently).
        if pkg.name == WORKSPACE_STUB_NAME {
            continue;
        }

        let id = CrateId {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
        };

        let (kind, lib_target) = pick_lib_target(pkg)?;
        let script_target = pkg
            .targets
            .iter()
            .find(|t| t.kind.iter().any(|k| k == "custom-build"));

        // manifest_dir is the parent of manifest_path. cargo always
        // emits absolute manifest_paths so this is unambiguous.
        let manifest_dir = pkg
            .manifest_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default();

        // Filter, sort, and lower the dep edges. One edge can be both
        // a normal and a build dep.
        let mut deps: Vec<ResolvedDep> = Vec::new();
        let mut build_deps: Vec<ResolvedDep> = Vec::new();
        for d in &node.deps {
            let normal = is_dep_active(d, None, platform)?;
            let build = script_target.is_some() && is_dep_active(d, Some("build"), host)?;
            if !normal && !build {
                continue;
            }
            let dep_pkg =
                by_id
                    .get(d.pkg.as_str())
                    .copied()
                    .ok_or_else(|| ParseError::MissingPackage {
                        pkg_id: d.pkg.clone(),
                    })?;
            let edge = ResolvedDep {
                extern_name: d.name.clone(),
                crate_id: CrateId {
                    name: dep_pkg.name.clone(),
                    version: dep_pkg.version.clone(),
                },
            };
            if build {
                build_deps.push(edge.clone());
            }
            if normal {
                deps.push(edge);
            }
        }
        deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));
        build_deps.sort_by(|a, b| a.extern_name.cmp(&b.extern_name));
        let build_script = script_target.map(|t| ResolvedBuildScript {
            path: t.src_path.clone(),
            edition: t.edition.clone().unwrap_or_else(|| pkg.edition.clone()),
            build_deps,
            package: PackageInfo {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                authors: pkg.authors.clone(),
                description: pkg.description.clone(),
                homepage: pkg.homepage.clone(),
                repository: pkg.repository.clone(),
                license: pkg.license.clone(),
                license_file: pkg.license_file.clone(),
                rust_version: pkg.rust_version.clone(),
                readme: pkg.readme.clone(),
                links: pkg.links.clone(),
                manifest_dir: manifest_dir.clone(),
                manifest_path: pkg.manifest_path.clone(),
            },
        });

        let mut features = node.features.clone();
        features.sort();
        features.dedup();

        crates.insert(
            id,
            ResolvedCrate {
                name: pkg.name.clone(),
                cargo_target_name: lib_target.name.replace('-', "_"),
                version: pkg.version.clone(),
                manifest_dir,
                lib_path: lib_target.src_path.clone(),
                edition: lib_target
                    .edition
                    .clone()
                    .unwrap_or_else(|| pkg.edition.clone()),
                kind,
                features,
                deps,
                build_script,
            },
        );
    }

    roots.sort();
    roots.dedup();

    Ok(ResolvedGraph {
        target,
        crates,
        roots,
    })
}

// -------------------- helpers --------------------

/// Locate the workspace stub package's PackageId in `workspace_members`.
/// Returns `None` if no member matches our stub name (possible when this
/// function is called against a metadata blob that wasn't generated
/// from a gluon-synthesised workspace; tests exercise that path).
fn workspace_stub_id<'a>(
    workspace_members: &'a [String],
    by_id: &BTreeMap<&str, &'a RawPackage>,
) -> Option<&'a str> {
    for id in workspace_members {
        if let Some(pkg) = by_id.get(id.as_str())
            && pkg.name == WORKSPACE_STUB_NAME
        {
            return Some(id.as_str());
        }
    }
    None
}

/// Pick the lib (or proc-macro) target out of a package's targets list.
///
/// A package with both `lib` and `proc-macro` is rejected — that
/// configuration is technically possible (a package whose lib target
/// has `proc-macro = true`) but ambiguous from gluon's perspective:
/// the artifact would need to be built for *both* host (proc-macro
/// loading) and the consumer's target (lib linking) and the spec
/// punts on that case.
fn pick_lib_target(pkg: &RawPackage) -> Result<(CrateKind, &RawTarget)> {
    let mut lib: Option<&RawTarget> = None;
    let mut proc_macro: Option<&RawTarget> = None;
    for t in &pkg.targets {
        // A target's `kind` array contains all roles cargo assigns to
        // it. `proc-macro` and `lib` may both appear on the same target
        // (the proc-macro lib target reports both); we treat that as
        // proc-macro since proc-macro is the load-bearing role.
        if t.kind.iter().any(|k| k == "proc-macro") {
            proc_macro = Some(t);
        } else if t.kind.iter().any(|k| k == "lib") {
            lib = Some(t);
        }
        // bin / example / test / bench / custom-build are intentionally
        // ignored — gluon only consumes libraries from vendored crates.
    }
    match (lib, proc_macro) {
        (Some(l), None) => Ok((CrateKind::Lib, l)),
        (None, Some(pm)) => Ok((CrateKind::ProcMacro, pm)),
        (Some(_), Some(_)) => Err(ParseError::AmbiguousLibKind {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
        }),
        (None, None) => Err(ParseError::NoLibTarget {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
        }),
    }
}

/// Decide whether a dep edge is active as a `kind` dep (`None` for
/// normal, `Some("build")` for build) on `platform`.
///
/// 1. Dev-deps are never active: vendored crates don't run tests
///    through gluon.
/// 2. A target-conditional entry is active only when its key (a
///    `cfg(...)` expression or a target name) matches `platform`, the
///    cfg set `rustc --print cfg` reports. Normal deps are matched
///    against the cross target, build deps against the host.
///
/// One edge can carry several `dep_kinds` entries (the same dep listed
/// under `[dependencies]` and a `[target.*.dependencies]` table); the
/// edge is active if any entry of `kind` applies.
fn is_dep_active(dep: &RawNodeDep, kind: Option<&str>, platform: &TargetPlatform) -> Result<bool> {
    if dep.dep_kinds.is_empty() {
        // Older cargo versions (< 1.41) didn't emit dep_kinds at all;
        // those edges are unconditional normal deps by definition.
        return Ok(kind.is_none());
    }
    for dk in dep.dep_kinds.iter().filter(|dk| dk.kind.as_deref() == kind) {
        let Some(key) = &dk.target else {
            return Ok(true);
        };
        let parsed = parse_platform(key).map_err(|_| ParseError::InvalidPlatform {
            dep: dep.name.clone(),
            key: key.clone(),
        })?;
        if platform.matches(&parsed) {
            return Ok(true);
        }
    }
    Ok(false)
}

// -------------------- tests --------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn target_handle() -> Handle<TargetDef> {
        Handle::<TargetDef>::new(0)
    }

    fn host_platform() -> TargetPlatform {
        TargetPlatform::from_cfg_lines(
            "x86_64-unknown-linux-gnu",
            ["target_arch=\"x86_64\"", "target_os=\"linux\"", "unix"],
        )
        .unwrap()
    }

    fn none_platform() -> TargetPlatform {
        TargetPlatform::from_cfg_lines(
            "x86_64-unknown-none",
            ["target_arch=\"x86_64\"", "target_os=\"none\""],
        )
        .unwrap()
    }

    /// Smallest possible graph: the workspace stub depends on a single
    /// no-deps lib crate. Mirrors the bitflags-only kernel scenario.
    const MINIMAL_JSON: &str = r#"
    {
      "packages": [
        {
          "id": "registry+x#bitflags@2.11.0",
          "name": "bitflags",
          "version": "2.11.0",
          "edition": "2021",
          "manifest_path": "/vendor/bitflags-2.11.0/Cargo.toml",
          "targets": [
            {
              "name": "bitflags",
              "kind": ["lib"],
              "src_path": "/vendor/bitflags-2.11.0/src/lib.rs",
              "edition": "2021"
            }
          ]
        },
        {
          "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
          "name": "gluon-vendor-workspace",
          "version": "0.0.0",
          "edition": "2021",
          "manifest_path": "/scratch/Cargo.toml",
          "targets": [
            {
              "name": "gluon-vendor-workspace",
              "kind": ["lib"],
              "src_path": "/scratch/lib.rs",
              "edition": "2021"
            }
          ]
        }
      ],
      "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
      "resolve": {
        "nodes": [
          {
            "id": "registry+x#bitflags@2.11.0",
            "deps": [],
            "features": []
          },
          {
            "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
            "deps": [
              {
                "name": "bitflags",
                "pkg": "registry+x#bitflags@2.11.0",
                "dep_kinds": [{"kind": null, "target": null}]
              }
            ],
            "features": []
          }
        ]
      }
    }
    "#;

    #[test]
    fn minimal_graph_drops_workspace_stub_and_lifts_root() {
        let g = parse_metadata(
            MINIMAL_JSON,
            target_handle(),
            &none_platform(),
            &host_platform(),
        )
        .expect("parse ok");
        assert_eq!(g.target, target_handle());
        assert_eq!(g.crates.len(), 1, "stub must be filtered out");
        assert_eq!(
            g.roots,
            vec![CrateId {
                name: "bitflags".into(),
                version: "2.11.0".into()
            }]
        );
        let bf = g
            .crates
            .get(&CrateId {
                name: "bitflags".into(),
                version: "2.11.0".into(),
            })
            .expect("bitflags present");
        assert_eq!(bf.cargo_target_name, "bitflags");
        assert_eq!(bf.kind, CrateKind::Lib);
        assert_eq!(bf.edition, "2021");
        assert!(bf.deps.is_empty());
        assert!(bf.build_script.is_none());
        assert_eq!(bf.manifest_dir, PathBuf::from("/vendor/bitflags-2.11.0"));
        assert_eq!(
            bf.lib_path,
            PathBuf::from("/vendor/bitflags-2.11.0/src/lib.rs")
        );
    }

    /// Graph with one transitive (bitflags + a fake leaf dep) and a
    /// proc-macro. Exercises hyphen normalisation, ProcMacro kind, and
    /// dep edge resolution.
    const TRANSITIVE_JSON: &str = r#"
    {
      "packages": [
        {
          "id": "registry+x#proc-macro2@1.0.0",
          "name": "proc-macro2",
          "version": "1.0.0",
          "edition": "2021",
          "manifest_path": "/vendor/proc-macro2-1.0.0/Cargo.toml",
          "targets": [
            {
              "name": "proc_macro2",
              "kind": ["proc-macro"],
              "src_path": "/vendor/proc-macro2-1.0.0/src/lib.rs",
              "edition": "2021"
            }
          ]
        },
        {
          "id": "registry+x#leaf@0.1.0",
          "name": "leaf",
          "version": "0.1.0",
          "edition": "2018",
          "manifest_path": "/vendor/leaf-0.1.0/Cargo.toml",
          "targets": [
            {
              "name": "leaf",
              "kind": ["lib"],
              "src_path": "/vendor/leaf-0.1.0/src/lib.rs"
            }
          ]
        },
        {
          "id": "registry+x#bitflags@2.11.0",
          "name": "bitflags",
          "version": "2.11.0",
          "edition": "2021",
          "manifest_path": "/vendor/bitflags-2.11.0/Cargo.toml",
          "targets": [
            {
              "name": "bitflags",
              "kind": ["lib"],
              "src_path": "/vendor/bitflags-2.11.0/src/lib.rs",
              "edition": "2021"
            }
          ]
        },
        {
          "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
          "name": "gluon-vendor-workspace",
          "version": "0.0.0",
          "edition": "2021",
          "manifest_path": "/scratch/Cargo.toml",
          "targets": [
            {"name": "gluon-vendor-workspace", "kind": ["lib"], "src_path": "/scratch/lib.rs"}
          ]
        }
      ],
      "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
      "resolve": {
        "nodes": [
          {
            "id": "registry+x#proc-macro2@1.0.0",
            "deps": [],
            "features": ["proc-macro"]
          },
          {
            "id": "registry+x#leaf@0.1.0",
            "deps": [],
            "features": []
          },
          {
            "id": "registry+x#bitflags@2.11.0",
            "deps": [
              {
                "name": "leaf",
                "pkg": "registry+x#leaf@0.1.0",
                "dep_kinds": [{"kind": null, "target": null}]
              },
              {
                "name": "pm",
                "pkg": "registry+x#proc-macro2@1.0.0",
                "dep_kinds": [{"kind": null, "target": null}]
              }
            ],
            "features": ["std", "alloc"]
          },
          {
            "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
            "deps": [
              {
                "name": "bitflags",
                "pkg": "registry+x#bitflags@2.11.0",
                "dep_kinds": [{"kind": null, "target": null}]
              }
            ],
            "features": []
          }
        ]
      }
    }
    "#;

    #[test]
    fn transitive_graph_keeps_dep_edges_and_normalises_proc_macro_name() {
        let g = parse_metadata(
            TRANSITIVE_JSON,
            target_handle(),
            &none_platform(),
            &host_platform(),
        )
        .expect("parse ok");
        assert_eq!(g.crates.len(), 3);
        assert_eq!(
            g.roots,
            vec![CrateId {
                name: "bitflags".into(),
                version: "2.11.0".into()
            }]
        );

        let pm = g
            .crates
            .get(&CrateId {
                name: "proc-macro2".into(),
                version: "1.0.0".into(),
            })
            .expect("proc-macro2 present");
        assert_eq!(pm.kind, CrateKind::ProcMacro);
        // Cargo target name already had no hyphens here, but it would
        // be underscore-normalised regardless.
        assert_eq!(pm.cargo_target_name, "proc_macro2");

        let bf = g
            .crates
            .get(&CrateId {
                name: "bitflags".into(),
                version: "2.11.0".into(),
            })
            .expect("bitflags present");
        assert_eq!(bf.deps.len(), 2);
        // Sorted by extern_name: "leaf" before "pm".
        assert_eq!(bf.deps[0].extern_name, "leaf");
        assert_eq!(bf.deps[0].crate_id.name, "leaf");
        assert_eq!(bf.deps[1].extern_name, "pm");
        assert_eq!(bf.deps[1].crate_id.name, "proc-macro2");
        assert_eq!(bf.features, vec!["alloc", "std"]);

        // Edition fallback: leaf has no per-target edition; package
        // edition (2018) must be used.
        let leaf = g
            .crates
            .get(&CrateId {
                name: "leaf".into(),
                version: "0.1.0".into(),
            })
            .expect("leaf present");
        assert_eq!(leaf.edition, "2018");
    }

    #[test]
    fn build_script_is_recorded() {
        let json = r#"
        {
          "packages": [
            {
              "id": "registry+x#has-build@1.0.0",
              "name": "has-build",
              "version": "1.0.0",
              "edition": "2021",
              "manifest_path": "/vendor/has-build-1.0.0/Cargo.toml",
              "targets": [
                {"name": "has_build", "kind": ["lib"], "src_path": "/vendor/has-build-1.0.0/src/lib.rs"},
                {"name": "build-script-build", "kind": ["custom-build"], "src_path": "/vendor/has-build-1.0.0/build.rs"}
              ]
            },
            {
              "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
              "name": "gluon-vendor-workspace",
              "version": "0.0.0",
              "edition": "2021",
              "manifest_path": "/scratch/Cargo.toml",
              "targets": [{"name": "stub", "kind": ["lib"], "src_path": "/scratch/lib.rs"}]
            }
          ],
          "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
          "resolve": {
            "nodes": [
              {"id": "registry+x#has-build@1.0.0", "deps": [], "features": []},
              {
                "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
                "deps": [{"name": "has_build", "pkg": "registry+x#has-build@1.0.0",
                          "dep_kinds": [{"kind": null, "target": null}]}],
                "features": []
              }
            ]
          }
        }
        "#;
        let g = parse_metadata(json, target_handle(), &none_platform(), &host_platform())
            .expect("parse ok");
        let hb = g
            .crates
            .get(&CrateId {
                name: "has-build".into(),
                version: "1.0.0".into(),
            })
            .expect("has-build present");
        let script = hb.build_script.as_ref().expect("build script recorded");
        assert_eq!(
            script.path,
            PathBuf::from("/vendor/has-build-1.0.0/build.rs")
        );
        assert_eq!(
            script.edition, "2021",
            "package edition when the target has none"
        );
        assert_eq!(script.package.name, "has-build");
        assert_eq!(
            script.package.manifest_path,
            PathBuf::from("/vendor/has-build-1.0.0/Cargo.toml")
        );
        assert_eq!(script.package.links, None);
        assert_eq!(hb.kind, CrateKind::Lib);
    }

    #[test]
    fn dev_and_build_dep_edges_are_dropped() {
        let json = r#"
        {
          "packages": [
            {
              "id": "registry+x#a@1.0.0", "name": "a", "version": "1.0.0",
              "edition": "2021", "manifest_path": "/vendor/a-1.0.0/Cargo.toml",
              "targets": [{"name": "a", "kind": ["lib"], "src_path": "/vendor/a-1.0.0/src/lib.rs"}]
            },
            {
              "id": "registry+x#dev_only@1.0.0", "name": "dev_only", "version": "1.0.0",
              "edition": "2021", "manifest_path": "/vendor/dev_only-1.0.0/Cargo.toml",
              "targets": [{"name": "dev_only", "kind": ["lib"], "src_path": "/vendor/dev_only-1.0.0/src/lib.rs"}]
            },
            {
              "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
              "name": "gluon-vendor-workspace", "version": "0.0.0", "edition": "2021",
              "manifest_path": "/scratch/Cargo.toml",
              "targets": [{"name": "stub", "kind": ["lib"], "src_path": "/scratch/lib.rs"}]
            }
          ],
          "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
          "resolve": {
            "nodes": [
              {"id": "registry+x#a@1.0.0",
               "deps": [{"name": "dev_only", "pkg": "registry+x#dev_only@1.0.0",
                         "dep_kinds": [{"kind": "dev", "target": null}]}],
               "features": []},
              {"id": "registry+x#dev_only@1.0.0", "deps": [], "features": []},
              {"id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
               "deps": [{"name": "a", "pkg": "registry+x#a@1.0.0",
                         "dep_kinds": [{"kind": null, "target": null}]}],
               "features": []}
            ]
          }
        }
        "#;
        let g = parse_metadata(json, target_handle(), &none_platform(), &host_platform())
            .expect("parse ok");
        let a = g
            .crates
            .get(&CrateId {
                name: "a".into(),
                version: "1.0.0".into(),
            })
            .unwrap();
        assert!(
            a.deps.is_empty(),
            "dev edge must be filtered, got: {:?}",
            a.deps
        );
    }

    #[test]
    fn build_deps_follow_host_cfgs_and_package_metadata_is_kept() {
        // `s` has a build script. Its edges: `both` (normal and build),
        // `cc` (build), `unix_tool` (build, cfg(unix): the host is
        // Linux), `win_tool` (build, cfg(windows)).
        let pkg = |name: &str| {
            format!(
                r#"{{"id": "registry+x#{name}@1.0.0", "name": "{name}", "version": "1.0.0",
                    "edition": "2021", "manifest_path": "/vendor/{name}/Cargo.toml",
                    "targets": [{{"name": "{name}", "kind": ["lib"], "src_path": "/vendor/{name}/src/lib.rs"}}]}}"#
            )
        };
        let edge = |name: &str, kinds: &str| {
            format!(
                r#"{{"name": "{name}", "pkg": "registry+x#{name}@1.0.0", "dep_kinds": [{kinds}]}}"#
            )
        };
        let leaf = |name: &str| {
            format!(r#"{{"id": "registry+x#{name}@1.0.0", "deps": [], "features": []}}"#)
        };
        let json = format!(
            r#"{{
              "packages": [
                {{"id": "registry+x#s@1.0.0", "name": "s", "version": "1.0.0-beta.1",
                  "edition": "2018", "manifest_path": "/vendor/s/Cargo.toml",
                  "links": "s", "authors": ["A <a@x>", "B"], "description": "d",
                  "license": "MIT", "rust_version": "1.70", "readme": "README.md",
                  "targets": [
                    {{"name": "s", "kind": ["lib"], "src_path": "/vendor/s/src/lib.rs"}},
                    {{"name": "build-script-build", "kind": ["custom-build"],
                      "src_path": "/vendor/s/build/main.rs", "edition": "2021"}}
                  ]}},
                {both}, {cc}, {unix_tool}, {win_tool},
                {{"id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
                  "name": "gluon-vendor-workspace", "version": "0.0.0", "edition": "2021",
                  "manifest_path": "/scratch/Cargo.toml",
                  "targets": [{{"name": "stub", "kind": ["lib"], "src_path": "/scratch/lib.rs"}}]}}
              ],
              "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
              "resolve": {{
                "nodes": [
                  {{"id": "registry+x#s@1.0.0", "features": [], "deps": [
                    {e_both}, {e_cc}, {e_unix}, {e_win}
                  ]}},
                  {l_both}, {l_cc}, {l_unix}, {l_win},
                  {{"id": "path+file:///scratch#gluon-vendor-workspace@0.0.0", "features": [],
                    "deps": [{{"name": "s", "pkg": "registry+x#s@1.0.0",
                              "dep_kinds": [{{"kind": null, "target": null}}]}}]}}
                ]
              }}
            }}"#,
            both = pkg("both"),
            cc = pkg("cc"),
            unix_tool = pkg("unix_tool"),
            win_tool = pkg("win_tool"),
            e_both = edge(
                "both",
                r#"{"kind": null, "target": null}, {"kind": "build", "target": null}"#
            ),
            e_cc = edge("cc", r#"{"kind": "build", "target": null}"#),
            e_unix = edge("unix_tool", r#"{"kind": "build", "target": "cfg(unix)"}"#),
            e_win = edge("win_tool", r#"{"kind": "build", "target": "cfg(windows)"}"#),
            l_both = leaf("both"),
            l_cc = leaf("cc"),
            l_unix = leaf("unix_tool"),
            l_win = leaf("win_tool"),
        );
        let g = parse_metadata(&json, target_handle(), &none_platform(), &host_platform())
            .expect("parse ok");
        let s = g
            .crates
            .get(&CrateId {
                name: "s".into(),
                version: "1.0.0-beta.1".into(),
            })
            .unwrap();
        let names = |deps: &[ResolvedDep]| -> Vec<String> {
            deps.iter().map(|d| d.extern_name.clone()).collect()
        };
        assert_eq!(names(&s.deps), ["both"]);
        let script = s.build_script.as_ref().unwrap();
        assert_eq!(names(&script.build_deps), ["both", "cc", "unix_tool"]);
        assert_eq!(script.path, PathBuf::from("/vendor/s/build/main.rs"));
        assert_eq!(script.edition, "2021");
        let p = &script.package;
        assert_eq!(p.version, "1.0.0-beta.1");
        assert_eq!(p.links.as_deref(), Some("s"));
        assert_eq!(p.authors, ["A <a@x>", "B"]);
        assert_eq!(p.description.as_deref(), Some("d"));
        assert_eq!(p.license.as_deref(), Some("MIT"));
        assert_eq!(p.rust_version.as_deref(), Some("1.70"));
        assert_eq!(p.readme.as_deref(), Some("README.md"));
        assert_eq!(p.homepage, None);
        assert_eq!(p.manifest_dir, PathBuf::from("/vendor/s"));
    }

    #[test]
    fn target_conditional_dep_follows_target_cfgs() {
        let json = r#"
        {
          "packages": [
            {
              "id": "registry+x#a@1.0.0", "name": "a", "version": "1.0.0",
              "edition": "2021", "manifest_path": "/vendor/a-1.0.0/Cargo.toml",
              "targets": [{"name": "a", "kind": ["lib"], "src_path": "/vendor/a-1.0.0/src/lib.rs"}]
            },
            {
              "id": "registry+x#unix_only@1.0.0", "name": "unix_only", "version": "1.0.0",
              "edition": "2021", "manifest_path": "/vendor/unix_only-1.0.0/Cargo.toml",
              "targets": [{"name": "unix_only", "kind": ["lib"], "src_path": "/vendor/unix_only-1.0.0/src/lib.rs"}]
            },
            {
              "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
              "name": "gluon-vendor-workspace", "version": "0.0.0", "edition": "2021",
              "manifest_path": "/scratch/Cargo.toml",
              "targets": [{"name": "stub", "kind": ["lib"], "src_path": "/scratch/lib.rs"}]
            }
          ],
          "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
          "resolve": {
            "nodes": [
              {"id": "registry+x#a@1.0.0",
               "deps": [{"name": "unix_only", "pkg": "registry+x#unix_only@1.0.0",
                         "dep_kinds": [{"kind": null, "target": "cfg(unix)"}]}],
               "features": []},
              {"id": "registry+x#unix_only@1.0.0", "deps": [], "features": []},
              {"id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
               "deps": [{"name": "a", "pkg": "registry+x#a@1.0.0",
                         "dep_kinds": [{"kind": null, "target": null}]}],
               "features": []}
            ]
          }
        }
        "#;
        let g = parse_metadata(json, target_handle(), &none_platform(), &host_platform())
            .expect("parse ok");
        let a = g
            .crates
            .get(&CrateId {
                name: "a".into(),
                version: "1.0.0".into(),
            })
            .unwrap();
        assert!(a.deps.is_empty(), "cfg(unix) edge must be filtered");

        // Keys that apply to x86_64-unknown-none keep the edge.
        for key in [r#"cfg(target_os = \"none\")"#, "x86_64-unknown-none"] {
            let json = json.replace("cfg(unix)", key);
            let g = parse_metadata(&json, target_handle(), &none_platform(), &host_platform())
                .expect("parse ok");
            let a = g.crates.values().find(|c| c.name == "a").unwrap();
            assert_eq!(a.deps.len(), 1, "edge with key {key} must be kept");
            assert_eq!(a.deps[0].extern_name, "unix_only");
        }

        let json = json.replace("cfg(unix)", "cfg(unix");
        let err = parse_metadata(&json, target_handle(), &none_platform(), &host_platform())
            .expect_err("bad key");
        assert!(
            matches!(err, ParseError::InvalidPlatform { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn missing_resolve_section_errors() {
        let json = r#"
        {
          "packages": [],
          "workspace_members": []
        }
        "#;
        let err = parse_metadata(json, target_handle(), &none_platform(), &host_platform())
            .expect_err("must error");
        assert!(matches!(err, ParseError::MissingResolve), "got: {err:?}");
    }

    #[test]
    fn ambiguous_lib_and_proc_macro_targets_error() {
        // A single package that declares BOTH a `lib` target and a
        // separate `proc-macro` target. Real-world packages don't do
        // this, but we want a precise error if cargo ever surfaces it.
        let json = r#"
        {
          "packages": [
            {
              "id": "registry+x#weird@1.0.0",
              "name": "weird",
              "version": "1.0.0",
              "edition": "2021",
              "manifest_path": "/vendor/weird-1.0.0/Cargo.toml",
              "targets": [
                {"name": "weird", "kind": ["lib"], "src_path": "/vendor/weird-1.0.0/src/lib.rs"},
                {"name": "weird_pm", "kind": ["proc-macro"], "src_path": "/vendor/weird-1.0.0/src/pm.rs"}
              ]
            },
            {
              "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
              "name": "gluon-vendor-workspace", "version": "0.0.0", "edition": "2021",
              "manifest_path": "/scratch/Cargo.toml",
              "targets": [{"name": "stub", "kind": ["lib"], "src_path": "/scratch/lib.rs"}]
            }
          ],
          "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
          "resolve": {
            "nodes": [
              {"id": "registry+x#weird@1.0.0", "deps": [], "features": []},
              {"id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
               "deps": [{"name": "weird", "pkg": "registry+x#weird@1.0.0",
                         "dep_kinds": [{"kind": null, "target": null}]}],
               "features": []}
            ]
          }
        }
        "#;
        let err = parse_metadata(json, target_handle(), &none_platform(), &host_platform())
            .expect_err("must error");
        assert!(
            matches!(err, ParseError::AmbiguousLibKind { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn no_lib_target_errors() {
        // bin-only crate (e.g. a CLI tool accidentally included as a dep).
        let json = r#"
        {
          "packages": [
            {
              "id": "registry+x#binonly@1.0.0",
              "name": "binonly",
              "version": "1.0.0",
              "edition": "2021",
              "manifest_path": "/vendor/binonly-1.0.0/Cargo.toml",
              "targets": [
                {"name": "binonly", "kind": ["bin"], "src_path": "/vendor/binonly-1.0.0/src/main.rs"}
              ]
            },
            {
              "id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
              "name": "gluon-vendor-workspace", "version": "0.0.0", "edition": "2021",
              "manifest_path": "/scratch/Cargo.toml",
              "targets": [{"name": "stub", "kind": ["lib"], "src_path": "/scratch/lib.rs"}]
            }
          ],
          "workspace_members": ["path+file:///scratch#gluon-vendor-workspace@0.0.0"],
          "resolve": {
            "nodes": [
              {"id": "registry+x#binonly@1.0.0", "deps": [], "features": []},
              {"id": "path+file:///scratch#gluon-vendor-workspace@0.0.0",
               "deps": [{"name": "binonly", "pkg": "registry+x#binonly@1.0.0",
                         "dep_kinds": [{"kind": null, "target": null}]}],
               "features": []}
            ]
          }
        }
        "#;
        let err = parse_metadata(json, target_handle(), &none_platform(), &host_platform())
            .expect_err("must error");
        assert!(
            matches!(err, ParseError::NoLibTarget { .. }),
            "got: {err:?}"
        );
    }
}
