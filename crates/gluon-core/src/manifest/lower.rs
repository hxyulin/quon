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
//! - root units (the lib, bins and `harness = false` tests of a root
//!   package in the root's domain) join the root's group and take the
//!   root's Gluon-only settings: linker script, cfg and rustc flags,
//!   `artifact_env`, `requires_config`, and the raw-binary crate type
//!   for bins and tests;
//! - every other unit joins a synthetic group per domain,
//!   `cargo:<target>` or `cargo:host`, which links the sysroot but not
//!   the config crate.
//!
//! Edges come from the manifest. The crate's [`CargoUnitDef`] carries
//! what rustc needs beyond a declared crate: the lib or bin crate name,
//! the package identity (with the `Cargo.lock` checksum for registry
//! packages), declared features for `--check-cfg`, lints, the `CARGO_*`
//! environment, and the build script that runs for the unit.
//!
//! Host-test units (see [`super::resolve`]) join the synthetic
//! `cargo:host-test` group, which builds for the host. Their lib and
//! `harness = true` tests become [`CrateType::Test`] crates.
//!
//! Units only the tests need (the tests, their dev-dependencies, and
//! copies of dependencies the dev-dependencies change; see
//! [`super::resolve`]) carry [`CargoUnitDef::test_only`], and
//! `gluon build` leaves them out.
//!
//! ### Build scripts
//!
//! The resolver's [`UnitKind::BuildScript`] units do not become crates.
//! Each lib, proc-macro or bin unit of a package with a build script
//! carries a [`CargoBuildScriptDef`] instead: the script's unit name
//! (owners that compile it identically share it), source, edition,
//! `links`, and its build-dependencies as names of host crates. Unit
//! lowering (`crate::unit::lower`) turns it into a script unit and one
//! run per owner, as it does for `cfg.dependency()` crates. The run's
//! environment is assembled when the run is dispatched, from the
//! owner's features and platform, so the resolver's
//! [`BuildScriptRun::env`](super::resolve::BuildScriptRun) is not used
//! here; it holds the same values.
//!
//! ### Registry sources
//!
//! Registry packages are read from Cargo's registry cache, or from the
//! vendor directory named by `cfg.cargo_workspace(..).vendored(dir)`;
//! see [`super::sources`]. A registry edge whose source cannot be found
//! is an error only when the resolver activates it, so an optional
//! dependency no feature enables never is.
//!
//! ### What is rejected
//!
//! Two packages with the same `links` value in one domain, a root whose
//! `gluon.rs` crate type (`Bin` or `RawBinary`) the manifest does not
//! back with a bin target, lib crate types other than one of `lib`,
//! `rlib` or `staticlib`, a root in a `no_sysroot` group that has
//! dependencies, a crate declared with
//! `group.add` that depends on a manifest unit or lives in a workspace
//! member's directory (it would be described twice), and unit names that
//! collide with declared crates.
//!
//! ### Freshness
//!
//! Expansion runs on every invocation and reads manifests fresh, so its
//! output cannot be stale. The script binary never reads manifests, so
//! editing a `Cargo.toml` does not recompile `gluon.rs`.

use super::platform::{self, ProbeTarget, TargetPlatform};
use super::resolve::{Domain, Root, UnitGraph, UnitKind, package_env, resolve};
use super::sources::{RegistrySources, check_vendor_stamp};
use super::workspace::{Package, PackageIdx, PackageSet, PackageSource, load_workspace};
use crate::compile::RustcInfo;
use crate::error::{Diagnostic, Error, Result};
use gluon_model::{
    Arena, CargoBuildScriptDef, CargoLint, CargoPackageDef, CargoPackageSource, CargoUnitDef,
    CrateType, RawBuildModel, RawCrateDef, RawDepDef, RawGroupDef,
};
use semver::Version;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Prefix of the groups that hold units no root names.
pub const SYNTHETIC_GROUP_PREFIX: &str = "cargo:";

/// Expand the manifest roots in `raw`. A model without
/// `cfg.cargo_workspace(...)` and without roots is left untouched and
/// reads nothing. `cache_dir` (the build directory) holds the target cfg
/// cache and the registry source map; `None` caches nothing. Returns
/// warnings for the caller to print.
pub fn expand(
    raw: &mut RawBuildModel,
    project_root: &Path,
    rustc: &RustcInfo,
    cache_dir: Option<&Path>,
) -> Result<Vec<Diagnostic>> {
    let sources_cache = cache_dir.map(|d| d.join("cargo-sources.json"));
    let Some(set) = load(raw, project_root, sources_cache.as_deref())? else {
        return Ok(Vec::new());
    };
    let cfg_cache = cache_dir.map(|d| d.join("target-cfg.json"));
    let platforms = probe_platforms(raw, project_root, rustc, cfg_cache.as_deref())?;
    expand_with(raw, project_root, set, &platforms, &rustc.host_triple)
}

/// Load the workspace `raw` names, or `None` when it names none and has
/// no roots. Registry sources come from the vendor directory `raw`
/// names, else from Cargo's registry cache, with the source map cached
/// in `sources_cache`.
pub fn load(
    raw: &RawBuildModel,
    project_root: &Path,
    sources_cache: Option<&Path>,
) -> Result<Option<PackageSet>> {
    let roots: Vec<&RawCrateDef> = raw
        .crates
        .iter()
        .map(|(_, c)| c)
        .filter(|c| c.manifest.is_some())
        .collect();
    let Some(ws) = &raw.cargo_workspace else {
        if roots.is_empty() && raw.host_tests.is_empty() {
            return Ok(None);
        }
        let hint = "add `cfg.cargo_workspace(\"<dir>\")` to gluon.rs";
        return Err(Error::Diagnostics(
            roots
                .iter()
                .map(|c| {
                    Diagnostic::error(format!(
                        "`package(\"{}\")` needs a Cargo workspace; {hint}",
                        c.name
                    ))
                    .with_optional_span(c.span.clone())
                })
                .chain(raw.host_tests.iter().map(|name| {
                    Diagnostic::error(format!(
                        "`host_tests().package(\"{name}\")` needs a Cargo workspace; {hint}"
                    ))
                }))
                .collect(),
        ));
    };
    let ws_root = project_root.join(&ws.dir);
    let sources = match &ws.vendored {
        Some(dir) => {
            let vendor_dir = project_root.join(dir);
            check_vendor_stamp(&ws_root, &vendor_dir)?;
            RegistrySources::vendor_dir(&ws_root, &vendor_dir)?
        }
        None => RegistrySources::registry_cache(&ws_root, sources_cache)?,
    };
    Ok(Some(load_workspace(&ws_root, &sources)?))
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
    let mut warnings = set.warnings.clone();

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
    let mut host_tests = Vec::new();
    for name in &raw.host_tests {
        match set.member(name) {
            Some(p) if set.packages[p].is_proc_macro() => errors.push(Diagnostic::error(format!(
                "`host_tests().package(\"{name}\")`: Gluon cannot run the tests of a \
                 proc-macro package yet"
            ))),
            Some(p) => host_tests.push(p),
            None => errors.push(Diagnostic::error(format!(
                "`host_tests().package(\"{name}\")`: `{name}` is not a member of the Cargo \
                 workspace at {}",
                set.root.display()
            ))),
        }
    }
    if !errors.is_empty() {
        return Err(Error::Diagnostics(errors));
    }
    // Host tests resolve in their own domain with the host's cfgs.
    let mut platforms = platforms.clone();
    if !host_tests.is_empty()
        && let Some(host) = platforms.get(&Domain::Host).cloned()
    {
        platforms.insert(Domain::HostTest, host);
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
    let graph = resolve(&set, &resolver_roots, &host_tests, &platforms, host_triple)?;
    let names = graph.unit_names();
    let checksums: BTreeMap<(&str, &Version), Option<&str>> = set
        .lock
        .iter()
        .flat_map(|l| &l.packages)
        .filter(|p| p.is_registry())
        .map(|p| ((p.name.as_str(), &p.version), p.checksum.as_deref()))
        .collect();

    check_unsupported(&graph, &roots, &mut errors);
    for r in roots.iter().filter(|r| !host_tests.contains(&r.package)) {
        let pkg = &graph.packages[r.package];
        for t in pkg.tests.iter().filter(|t| t.harness) {
            warnings.push(
                Diagnostic::warning(format!(
                    "test `{}` of package `{}` uses the libtest harness, so Gluon skips it",
                    t.name,
                    pkg.name()
                ))
                .with_note(format!(
                    "add `[[test]] name = \"{}\"` with `harness = false` to {} to boot it, \
                     or `cfg.host_tests().package(\"{}\")` to gluon.rs to run it on the host",
                    t.name,
                    pkg.manifest_path.display(),
                    pkg.name()
                )),
            );
        }
    }
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
        // A script is compiled by unit lowering from its owners'
        // `CargoBuildScriptDef`; see the module docs.
        if u.kind == UnitKind::BuildScript {
            continue;
        }
        let pkg = &graph.packages[u.package];
        let root = if u.is_root {
            root_of(u.package, &u.domain)
        } else {
            None
        };
        let source = match pkg.id.source {
            PackageSource::Path(_) => CargoPackageSource::Path,
            PackageSource::Registry => CargoPackageSource::Registry {
                checksum: checksums
                    .get(&(pkg.name(), &pkg.id.version))
                    .copied()
                    .flatten()
                    .map(str::to_string),
            },
        };
        let script = u
            .build_script
            .as_ref()
            .map(|run| build_script_def(&graph, run.script, &names));
        let doctest_lib = u.doctest_lib.map(|l| names[l].clone());
        // `CARGO_BIN_EXE_<bin>` for a host test in `tests/`. The bin
        // builds first (`artifact_deps`), and its path is a freshness
        // input of the test (see `compile::env`).
        let bin_exes: Vec<(String, String)> = u
            .bins
            .iter()
            .map(|&b| {
                let name = match &graph.units[b].kind {
                    UnitKind::Bin(n) => n.clone(),
                    other => unreachable!("a test's bins are bin units, got {other}"),
                };
                (format!("CARGO_BIN_EXE_{name}"), names[b].clone())
            })
            .collect();
        match lower_unit(u, pkg, source, &names, root, project_root, script) {
            Ok(mut def) => {
                if let Some(c) = def.cargo.as_mut() {
                    c.doctest_lib = doctest_lib;
                }
                for (var, bin) in bin_exes {
                    def.artifact_deps.push(bin.clone());
                    def.artifact_env.insert(var, bin);
                }
                lowered.push(def)
            }
            Err(d) => errors.push(d),
        }
    }
    if !errors.is_empty() {
        return Err(Error::Diagnostics(errors));
    }

    replace_roots(raw, &roots, lowered)?;
    Ok(warnings)
}

/// Report what Gluon cannot build: two packages sharing a `links` value
/// in one domain, a root crate type the manifest does not back, and
/// dependencies of `no_sysroot` roots. Lib crate types are checked when
/// a unit is lowered.
fn check_unsupported(graph: &UnitGraph, roots: &[RootInfo], errors: &mut Vec<Diagnostic>) {
    // Cargo rejects this too: `DEP_<LINKS>_*` and the native library
    // would be ambiguous.
    let mut links: BTreeMap<(&Domain, &str), BTreeSet<PackageIdx>> = BTreeMap::new();
    for u in graph
        .units
        .iter()
        .filter(|u| u.kind != UnitKind::BuildScript)
    {
        if let Some(l) = &graph.packages[u.package].links {
            links
                .entry((&u.domain, l.as_str()))
                .or_default()
                .insert(u.package);
        }
    }
    for ((domain, value), packages) in &links {
        if packages.len() > 1 {
            let names: Vec<String> = packages
                .iter()
                .map(|&p| format!("`{}`", graph.packages[p].name()))
                .collect();
            errors.push(Diagnostic::error(format!(
                "packages {} all set `links = \"{value}\"` for {domain}; only one package \
                 may link a native library",
                names.join(", ")
            )));
        }
    }
    for r in roots {
        let pkg = &graph.packages[r.package];
        let wants_bin = matches!(r.def.crate_type, CrateType::Bin | CrateType::RawBinary);
        if wants_bin && pkg.bins.is_empty() {
            errors.push(
                Diagnostic::error(format!(
                    "package root '{}' has crate type {:?} in gluon.rs, but package `{}` has \
                     no bin target",
                    r.def.name,
                    r.def.crate_type,
                    pkg.name()
                ))
                .with_optional_span(r.def.span.clone())
                .with_note(format!(
                    "add a `[[bin]]` or `src/main.rs` to {}, or remove `crate_type` from \
                     gluon.rs",
                    pkg.manifest_path.display()
                )),
            );
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

/// The build script `script` (a [`UnitKind::BuildScript`] unit) as its
/// owners carry it.
fn build_script_def(graph: &UnitGraph, script: usize, names: &[String]) -> CargoBuildScriptDef {
    let s = &graph.units[script];
    let pkg = &graph.packages[s.package];
    let path = pkg
        .build_script
        .as_ref()
        .expect("build-script units come from a package with a build script");
    CargoBuildScriptDef {
        unit: names[script].clone(),
        path: path
            .strip_prefix(pkg.manifest_dir())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned(),
        edition: pkg.edition.clone(),
        deps: s
            .deps
            .iter()
            .map(|d| (d.extern_name.clone(), names[d.unit].clone()))
            .collect(),
        links: pkg.links.clone(),
    }
}

/// Lower one unit to a crate definition.
/// `CARGO_TARGET_TMPDIR` of host tests in `tests/`, relative to the
/// project root.
pub const HOST_TEST_TMPDIR: &str = "build/tmp";

fn lower_unit(
    u: &super::resolve::Unit,
    pkg: &Package,
    source: CargoPackageSource,
    names: &[String],
    root: Option<&RootInfo>,
    project_root: &Path,
    build_script: Option<CargoBuildScriptDef>,
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
        UnitKind::Test(name) => {
            let t = pkg
                .tests
                .iter()
                .find(|t| &t.name == name)
                .expect("test units come from a test target");
            let ty = match root.map(|r| r.def.crate_type) {
                Some(CrateType::RawBinary) => CrateType::RawBinary,
                _ => CrateType::Bin,
            };
            (ty, &t.path, &t.edition, name.replace('-', "_"), None)
        }
        UnitKind::BinTest(name) => {
            let b = pkg
                .bins
                .iter()
                .find(|b| &b.name == name)
                .expect("bin-test units come from a bin target");
            (
                CrateType::Test,
                &b.path,
                &b.edition,
                name.replace('-', "_"),
                Some(name),
            )
        }
        UnitKind::LibTest => {
            let lib = pkg
                .lib
                .as_ref()
                .expect("lib-test units come from a lib target");
            (
                CrateType::Test,
                &lib.path,
                &lib.edition,
                lib.name.replace('-', "_"),
                None,
            )
        }
        UnitKind::HarnessTest(name) => {
            let t = pkg
                .tests
                .iter()
                .find(|t| &t.name == name)
                .expect("harness-test units come from a test target");
            (
                CrateType::Test,
                &t.path,
                &t.edition,
                name.replace('-', "_"),
                None,
            )
        }
        other => {
            // The caller skips build-script units.
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
    // Cargo sets it for the packages named on its command line, which
    // a host-test root is.
    if u.is_root
        || matches!(
            u.kind,
            UnitKind::LibTest | UnitKind::BinTest(_) | UnitKind::HarnessTest(_)
        )
    {
        env.insert("CARGO_PRIMARY_PACKAGE".into(), "1".into());
    }
    if let Some(bin) = bin {
        env.insert("CARGO_BIN_NAME".into(), bin.clone());
    }
    // Cargo gives tests in `tests/` a scratch directory under its
    // target directory; Gluon's is `build/tmp`, which `gluon test`
    // creates before running them (see `run::host`).
    if matches!(u.kind, UnitKind::HarnessTest(_)) {
        env.insert(
            "CARGO_TARGET_TMPDIR".into(),
            project_root
                .join(HOST_TEST_TMPDIR)
                .to_string_lossy()
                .into_owned(),
        );
    }

    let manifest_dir = relative_to(dir, project_root);
    let src_rel = src
        .strip_prefix(dir)
        .unwrap_or(src)
        .to_string_lossy()
        .into_owned();
    let target = match &u.domain {
        Domain::Host | Domain::HostTest => "host".to_string(),
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
                source,
            },
            declared_features: pkg.features.keys().cloned().collect(),
            check_cfg,
            lints,
            env,
            build_script,
            is_test: matches!(u.kind, UnitKind::Test(_)),
            test_only: u.test_only,
            host_test: u.domain == Domain::HostTest,
            // Filled in by the caller, which sees the other units.
            doctest_lib: None,
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
    // Cargo passes `--cfg test` to `harness = false` tests too.
    if matches!(u.kind, UnitKind::Test(_)) {
        def.cfg_flags.push("test".into());
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
