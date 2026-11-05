//! Manifest expansion: turn `group.package(...)` roots into crates.
//!
//! [`expand`] runs in `script_runner::finalize`, after `gluon.rs` has
//! produced its [`RawBuildModel`] and before intern. It reads the Cargo
//! workspace named by `cfg.cargo_workspace(dir)`, resolves features per
//! domain for the roots, and replaces each root with the units the
//! resolver produced. Every later stage (intern, `config::resolve`, unit
//! lowering, the DAG) sees one uniform set of crates.
//!
//! ### What each unit becomes
//!
//! One [`RawCrateDef`] per unit, named by [`UnitGraph::unit_names`]:
//!
//! - root units (the lib and bins of a root package in the root's
//!   domain) join the root's group and take the root's Gluon-only
//!   settings: linker script, cfg and rustc flags, `artifact_env`,
//!   `requires_config`, and the raw-binary crate type for bins;
//! - every other unit joins a synthetic group per domain,
//!   `cargo:<target>` or `cargo:host`, which links the sysroot but not
//!   the config crate.
//!
//! Edges come from the manifest. The crate's [`CargoUnitDef`] carries
//! what rustc needs beyond a declared crate: the lib or bin crate name,
//! the package identity, declared features for `--check-cfg`, lints, and
//! the `CARGO_*` environment.
//!
//! ### What is rejected
//!
//! Registry dependencies (not yet supported), build scripts and `links`
//! (until build-script support lands), lib crate types other than one of
//! `lib`, `rlib` or `staticlib`, a root in a `no_sysroot` group that has
//! dependencies, a crate declared with `group.add` that depends on a
//! manifest unit or lives in a workspace member's directory (it would be
//! described twice), and unit names that collide with declared crates.
//!
//! ### Freshness
//!
//! Expansion runs on every invocation and reads manifests fresh, so its
//! output cannot be stale. The script binary never reads manifests, so
//! editing a `Cargo.toml` does not recompile `gluon.rs`.

use super::platform::{self, ProbeTarget, TargetPlatform};
use super::resolve::{Domain, Root, UnitGraph, UnitKind, package_env, resolve};
use super::sources::RegistrySources;
use super::workspace::{DepSource, DepTarget, Package, PackageIdx, PackageSet, load_workspace};
use crate::compile::RustcInfo;
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{
    Arena, CargoLint, CargoPackageDef, CargoUnitDef, CrateType, RawBuildModel, RawCrateDef,
    RawDepDef, RawGroupDef,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Prefix of the groups that hold units no root names.
pub const SYNTHETIC_GROUP_PREFIX: &str = "cargo:";

/// Expand the manifest roots in `raw`. A model without
/// `cfg.cargo_workspace(...)` and without roots is left untouched and
/// reads nothing. Returns warnings for the caller to print.
pub fn expand(
    raw: &mut RawBuildModel,
    project_root: &Path,
    rustc: &RustcInfo,
    cfg_cache: Option<&Path>,
) -> Result<Vec<Diagnostic>> {
    let Some(set) = load(raw, project_root)? else {
        return Ok(Vec::new());
    };
    let platforms = probe_platforms(raw, project_root, rustc, cfg_cache)?;
    expand_with(raw, project_root, set, &platforms, &rustc.host_triple)
}

/// Load the workspace `raw` names, or `None` when it names none and has
/// no roots.
pub fn load(raw: &RawBuildModel, project_root: &Path) -> Result<Option<PackageSet>> {
    let roots: Vec<&RawCrateDef> = raw
        .crates
        .iter()
        .map(|(_, c)| c)
        .filter(|c| c.manifest.is_some())
        .collect();
    let Some(ws) = &raw.cargo_workspace else {
        if roots.is_empty() {
            return Ok(None);
        }
        return Err(Error::Diagnostics(
            roots
                .iter()
                .map(|c| {
                    Diagnostic::error(format!(
                        "`package(\"{}\")` needs a Cargo workspace; add \
                         `cfg.cargo_workspace(\"<dir>\")` to gluon.rs",
                        c.name
                    ))
                    .with_optional_span(c.span.clone())
                })
                .collect(),
        ));
    };
    // Registry dependencies are mapped through an empty source map, so
    // each one is recorded as unresolved; `reject_registry_deps` then
    // gives it a clear reason, reported only if the resolver activates
    // the edge.
    let sources = RegistrySources::new(BTreeMap::new(), "");
    let mut set = load_workspace(&project_root.join(&ws.dir), &sources)?;
    reject_registry_deps(&mut set);
    Ok(Some(set))
}

/// Mark every registry edge unresolved with the reason the build
/// cannot use it yet.
fn reject_registry_deps(set: &mut PackageSet) {
    for pkg in &mut set.packages {
        for dep in &mut pkg.deps {
            if let DepSource::Registry(req) = &dep.source {
                dep.target = DepTarget::Unresolved(format!(
                    "registry dependency `{} {req}` is not supported yet; Gluon builds \
                     path dependencies only",
                    dep.package
                ));
            }
        }
    }
}

/// The domain a group's crates build in.
fn group_domain(group: &RawGroupDef) -> Domain {
    if group.target == "host" {
        Domain::Host
    } else {
        Domain::Target(group.target.clone())
    }
}

/// Probe the cfg set of the host and of every target a root builds
/// for.
fn probe_platforms(
    raw: &RawBuildModel,
    project_root: &Path,
    rustc: &RustcInfo,
    cfg_cache: Option<&Path>,
) -> Result<BTreeMap<Domain, TargetPlatform>> {
    let mut platforms = BTreeMap::new();
    platforms.insert(
        Domain::Host,
        platform::probe(rustc, &ProbeTarget::Host, cfg_cache)?,
    );
    for (_, c) in raw.crates.iter().filter(|(_, c)| c.manifest.is_some()) {
        let Some(group) = raw.groups.lookup(&c.group).and_then(|h| raw.groups.get(h)) else {
            continue;
        };
        let domain = group_domain(group);
        let Domain::Target(name) = &domain else {
            continue;
        };
        if platforms.contains_key(&domain) {
            continue;
        }
        // An unknown target is reported by intern; no domain for it.
        let Some(t) = raw.targets.lookup(name).and_then(|h| raw.targets.get(h)) else {
            continue;
        };
        let probe = ProbeTarget::for_target(t, project_root);
        platforms.insert(domain, platform::probe(rustc, &probe, cfg_cache)?);
    }
    Ok(platforms)
}

/// One `group.package(...)` root.
struct RootInfo {
    /// The root's `RawCrateDef`, whose settings root units take.
    def: RawCrateDef,
    package: PackageIdx,
    domain: Domain,
    no_sysroot: bool,
}

/// [`expand`] with the workspace loaded and the platforms probed.
/// `platforms` must hold [`Domain::Host`] and every root's domain.
pub fn expand_with(
    raw: &mut RawBuildModel,
    project_root: &Path,
    set: PackageSet,
    platforms: &BTreeMap<Domain, TargetPlatform>,
    host_triple: &str,
) -> Result<Vec<Diagnostic>> {
    let mut errors = Vec::new();
    let warnings = set.warnings.clone();

    let mut roots: Vec<RootInfo> = Vec::new();
    for (_, c) in raw.crates.iter() {
        let Some(m) = &c.manifest else { continue };
        let Some(group) = raw.groups.lookup(&c.group).and_then(|h| raw.groups.get(h)) else {
            // Intern reports the unknown group.
            continue;
        };
        let Some(package) = set.member(&m.package) else {
            errors.push(
                Diagnostic::error(format!(
                    "package `{}` is not a member of the Cargo workspace at {}",
                    m.package,
                    set.root.display()
                ))
                .with_optional_span(c.span.clone()),
            );
            continue;
        };
        roots.push(RootInfo {
            def: c.clone(),
            package,
            domain: group_domain(group),
            no_sysroot: group.no_sysroot,
        });
    }
    if !errors.is_empty() {
        return Err(Error::Diagnostics(errors));
    }

    let resolver_roots: Vec<Root> = roots
        .iter()
        .map(|r| {
            let m = r.def.manifest.as_ref().expect("roots have a manifest");
            Root {
                package: r.package,
                domain: r.domain.clone(),
                features: m.features.clone(),
                default_features: m.default_features,
            }
        })
        .collect();
    let graph = resolve(&set, &resolver_roots, platforms, host_triple)?;
    let names = graph.unit_names();

    check_unsupported(&graph, &roots, &mut errors);
    check_declared_crates(raw, &set, &graph, &names, &roots, project_root, &mut errors);
    if !errors.is_empty() {
        return Err(Error::Diagnostics(errors));
    }

    let root_of = |package: PackageIdx, domain: &Domain| {
        roots
            .iter()
            .find(|r| r.package == package && &r.domain == domain)
    };

    let mut lowered: Vec<RawCrateDef> = Vec::with_capacity(graph.units.len());
    for u in &graph.units {
        let pkg = &graph.packages[u.package];
        let root = if u.is_root {
            root_of(u.package, &u.domain)
        } else {
            None
        };
        match lower_unit(u, pkg, &names, root, project_root) {
            Ok(def) => lowered.push(def),
            Err(d) => errors.push(d),
        }
    }
    if !errors.is_empty() {
        return Err(Error::Diagnostics(errors));
    }

    replace_roots(raw, &roots, lowered)?;
    Ok(warnings)
}

/// Report what stage 3 cannot build: build scripts, `links`, lib crate
/// types, and dependencies of `no_sysroot` roots.
fn check_unsupported(graph: &UnitGraph, roots: &[RootInfo], errors: &mut Vec<Diagnostic>) {
    let active: BTreeSet<PackageIdx> = graph.units.iter().map(|u| u.package).collect();
    for &p in &active {
        let pkg = &graph.packages[p];
        if let Some(script) = &pkg.build_script {
            errors.push(
                Diagnostic::error(format!(
                    "package `{}` has a build script, which Gluon does not run yet",
                    pkg.name()
                ))
                .with_note(format!("build script: {}", script.display())),
            );
        }
        if let Some(links) = &pkg.links {
            errors.push(Diagnostic::error(format!(
                "package `{}` sets `links = \"{links}\"`, which Gluon does not support",
                pkg.name()
            )));
        }
    }
    for r in roots.iter().filter(|r| r.no_sysroot) {
        let has_deps = graph
            .units
            .iter()
            .filter(|u| u.package == r.package && u.domain == r.domain)
            .any(|u| {
                u.deps
                    .iter()
                    .any(|d| graph.units[d.unit].package != r.package)
            });
        if has_deps {
            errors.push(
                Diagnostic::error(format!(
                    "package `{}` is in `no_sysroot` group '{}' but has dependencies; they \
                     would need to be `no_core` too",
                    graph.packages[r.package].name(),
                    r.def.group
                ))
                .with_optional_span(r.def.span.clone()),
            );
        }
    }
}

/// Reject crates declared with `group.add` that overlap the workspace:
/// a name taken by a unit, an edge into a manifest unit, or a crate
/// directory that is a workspace member's.
fn check_declared_crates(
    raw: &RawBuildModel,
    set: &PackageSet,
    graph: &UnitGraph,
    names: &[String],
    roots: &[RootInfo],
    project_root: &Path,
    errors: &mut Vec<Diagnostic>,
) {
    let unit_names: BTreeMap<&str, &str> = graph
        .units
        .iter()
        .map(|u| (names[u.id].as_str(), graph.packages[u.package].name()))
        .collect();
    let root_names: BTreeSet<&str> = roots.iter().map(|r| r.def.name.as_str()).collect();
    let member_dirs: BTreeMap<PathBuf, &str> = set
        .members
        .iter()
        .map(|&i| {
            let p = &set.packages[i];
            (normalize(p.manifest_dir()), p.name())
        })
        .collect();
    for (_, c) in raw.crates.iter().filter(|(_, c)| c.manifest.is_none()) {
        if let Some(pkg) = unit_names.get(c.name.as_str()) {
            errors.push(
                Diagnostic::error(format!(
                    "crate '{}' is declared with `group.add`, and package `{pkg}` in the \
                     Cargo workspace has a unit of the same name",
                    c.name
                ))
                .with_optional_span(c.span.clone()),
            );
        }
        for (extern_name, dep) in &c.deps {
            let target = dep.crate_name.as_str();
            if unit_names.contains_key(target) || root_names.contains(target) {
                errors.push(
                    Diagnostic::error(format!(
                        "crate '{}' (declared with `group.add`) depends on '{target}' \
                         (as `{extern_name}`), which comes from the Cargo workspace; \
                         declare '{}' as a workspace package instead",
                        c.name, c.name
                    ))
                    .with_optional_span(dep.span.clone().or_else(|| c.span.clone())),
                );
            }
        }
        let dir = normalize(&project_root.join(&c.path));
        if let Some(pkg) = member_dirs.get(&dir) {
            errors.push(
                Diagnostic::error(format!(
                    "crate '{}' is declared with `group.add` at {}, which is the directory of \
                     workspace member `{pkg}`; use `package(\"{pkg}\")` instead",
                    c.name, c.path
                ))
                .with_optional_span(c.span.clone()),
            );
        }
    }
}

/// `p` with `.` and `..` components removed lexically, and canonical
/// when it exists, so two spellings of one directory compare equal.
fn normalize(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `p` relative to `base` when it lies under it, else `p` itself. The
/// workspace root is canonical, so `base` is canonicalised for the
/// comparison.
fn relative_to(p: &Path, base: &Path) -> String {
    let base = std::fs::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    let rel = p.strip_prefix(&base).unwrap_or(p).to_string_lossy();
    if rel.is_empty() {
        ".".into()
    } else {
        rel.into_owned()
    }
}

fn synthetic_group(domain: &Domain) -> String {
    format!("{SYNTHETIC_GROUP_PREFIX}{domain}")
}

/// Map a lib's `crate-type` list to a Gluon crate type.
fn lib_crate_type(
    pkg: &Package,
    crate_types: &[String],
) -> std::result::Result<CrateType, Diagnostic> {
    match crate_types {
        [] => Ok(CrateType::Lib),
        [t] if t == "lib" || t == "rlib" => Ok(CrateType::Lib),
        [t] if t == "staticlib" => Ok(CrateType::StaticLib),
        _ => Err(Diagnostic::error(format!(
            "package `{}`: lib crate type `{}` is not supported; use one of `lib`, `rlib` \
             or `staticlib`",
            pkg.name(),
            crate_types.join(", ")
        ))),
    }
}

/// Lower one unit to a crate definition.
fn lower_unit(
    u: &super::resolve::Unit,
    pkg: &Package,
    names: &[String],
    root: Option<&RootInfo>,
    project_root: &Path,
) -> std::result::Result<RawCrateDef, Diagnostic> {
    let dir = pkg.manifest_dir();
    let (crate_type, src, edition, rustc_name, bin) = match &u.kind {
        UnitKind::Lib | UnitKind::ProcMacro => {
            let lib = pkg.lib.as_ref().expect("lib units come from a lib target");
            let ty = if lib.proc_macro {
                CrateType::ProcMacro
            } else {
                lib_crate_type(pkg, &lib.crate_types)?
            };
            (
                ty,
                &lib.path,
                &lib.edition,
                lib.name.replace('-', "_"),
                None,
            )
        }
        UnitKind::Bin(name) => {
            let b = pkg
                .bins
                .iter()
                .find(|b| &b.name == name)
                .expect("bin units come from a bin target");
            let ty = match root.map(|r| r.def.crate_type) {
                Some(CrateType::RawBinary) => CrateType::RawBinary,
                _ => CrateType::Bin,
            };
            (ty, &b.path, &b.edition, name.replace('-', "_"), Some(name))
        }
        other => {
            // The resolver makes test units only in test mode (stage 6),
            // and build scripts are rejected before lowering.
            return Err(Diagnostic::error(format!(
                "package `{}`: {other} units are not supported",
                pkg.name()
            )));
        }
    };

    let mut lints = Vec::new();
    let mut check_cfg = Vec::new();
    for (tool, entries) in &pkg.lints {
        for (name, l) in entries {
            if !matches!(l.level.as_str(), "allow" | "warn" | "deny" | "forbid") {
                return Err(Diagnostic::error(format!(
                    "package `{}`: lint `{tool}::{name}` has level `{}`; Gluon supports \
                     allow, warn, deny and forbid",
                    pkg.name(),
                    l.level
                ))
                .with_note(format!("in {}", pkg.manifest_path.display())));
            }
            if tool == "rust" && name == "unexpected_cfgs" {
                check_cfg.extend(l.check_cfg.iter().cloned());
            }
            lints.push(CargoLint {
                tool: tool.clone(),
                name: name.clone(),
                level: l.level.clone(),
                priority: l.priority,
            });
        }
    }
    // Cargo's order: priority, then the lint's full name (`clippy::all`
    // after `unsafe_code`), so later flags override earlier ones the
    // same way.
    let full = |l: &CargoLint| {
        if l.tool == "rust" {
            l.name.clone()
        } else {
            format!("{}::{}", l.tool, l.name)
        }
    };
    lints.sort_by_key(|l| (l.priority, full(l)));

    let mut env = package_env(pkg);
    env.insert("CARGO_CRATE_NAME".into(), rustc_name.clone());
    if u.is_root {
        env.insert("CARGO_PRIMARY_PACKAGE".into(), "1".into());
    }
    if let Some(bin) = bin {
        env.insert("CARGO_BIN_NAME".into(), bin.clone());
    }

    let manifest_dir = relative_to(dir, project_root);
    let src_rel = src
        .strip_prefix(dir)
        .unwrap_or(src)
        .to_string_lossy()
        .into_owned();
    let target = match &u.domain {
        Domain::Host => "host".to_string(),
        Domain::Target(t) => t.clone(),
    };
    let deps = u
        .deps
        .iter()
        .map(|d| {
            (
                d.extern_name.clone(),
                RawDepDef {
                    crate_name: names[d.unit].clone(),
                    ..Default::default()
                },
            )
        })
        .collect();

    let mut def = RawCrateDef {
        name: names[u.id].clone(),
        path: manifest_dir.clone(),
        edition: edition.clone(),
        crate_type,
        target,
        deps,
        features: u.features.iter().cloned().collect(),
        root: Some(src_rel),
        group: synthetic_group(&u.domain),
        is_project_crate: pkg.is_member,
        cargo: Some(CargoUnitDef {
            rustc_name,
            package: CargoPackageDef {
                name: pkg.name().to_string(),
                version: pkg.id.version.to_string(),
                manifest_dir,
            },
            declared_features: pkg.features.keys().cloned().collect(),
            check_cfg,
            lints,
            env,
        }),
        ..Default::default()
    };
    if let Some(r) = root {
        let rd = &r.def;
        def.group = rd.group.clone();
        def.linker_script = rd.linker_script.clone();
        def.cfg_flags = rd.cfg_flags.clone();
        def.rustc_flags = rd.rustc_flags.clone();
        def.requires_config = rd.requires_config.clone();
        def.artifact_deps = rd.artifact_deps.clone();
        def.artifact_env = rd.artifact_env.clone();
        def.span = rd.span.clone();
    }
    Ok(def)
}

/// Swap the roots in `raw` for the lowered units: rebuild the crate
/// arena (declared crates first, in their order, then units in unit
/// order), point the roots' groups at their units, and add one
/// synthetic group per domain that has non-root units.
fn replace_roots(
    raw: &mut RawBuildModel,
    roots: &[RootInfo],
    lowered: Vec<RawCrateDef>,
) -> Result<()> {
    let old = std::mem::take(&mut raw.crates);
    let mut crates = Arena::new();
    for (_, c) in old.iter().filter(|(_, c)| c.manifest.is_none()) {
        crates.insert(c.name.clone(), c.clone());
    }

    let root_names: BTreeSet<&str> = roots.iter().map(|r| r.def.name.as_str()).collect();
    let mut members: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut synthetic: BTreeMap<String, String> = BTreeMap::new();
    for def in lowered {
        if def.group.starts_with(SYNTHETIC_GROUP_PREFIX) {
            synthetic
                .entry(def.group.clone())
                .or_insert_with(|| def.target.clone());
        }
        members
            .entry(def.group.clone())
            .or_default()
            .push(def.name.clone());
        let name = def.name.clone();
        let (_, inserted) = crates.insert(name.clone(), def);
        if !inserted {
            // `check_declared_crates` rejects collisions with declared
            // crates and unit names are unique, so this is a bug.
            return Err(Error::Config(format!(
                "manifest expansion produced crate '{name}' twice (lowering bug)"
            )));
        }
    }
    raw.crates = crates;

    for (name, target) in &synthetic {
        if raw.groups.lookup(name).is_some() {
            return Err(Error::Diagnostics(vec![Diagnostic::error(format!(
                "group name '{name}' is reserved for Cargo workspace dependencies"
            ))]));
        }
        raw.groups.insert(
            name.clone(),
            RawGroupDef {
                name: name.clone(),
                target: target.clone(),
                // Units carry their own edition.
                default_edition: "2015".into(),
                crates: Vec::new(),
                shared_flags: Vec::new(),
                is_project: true,
                config: false,
                no_sysroot: false,
                span: None,
            },
        );
    }
    let handles: Vec<_> = raw.groups.iter().map(|(h, _)| h).collect();
    for h in handles {
        let g = raw.groups.get_mut(h).expect("handle from iter");
        g.crates.retain(|c| !root_names.contains(c.as_str()));
        if let Some(names) = members.remove(&g.name) {
            g.crates.extend(names);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
