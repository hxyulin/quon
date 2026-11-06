//! Feature resolver (Cargo resolver 2 semantics) and the [`UnitGraph`].
//!
//! Features are resolved per [`Domain`]: one per Quon target with roots,
//! plus `Host` for proc macros, build dependencies, everything below
//! them, and host roots. Within a domain a package has one feature set,
//! the union of every request that reaches it. An edge into a proc-macro
//! package or a build-dependency edge moves into `Host`.
//!
//! The resolver is a FIFO worklist over feature requests. State only
//! grows (enabled features, activated dependency edges, stored weak
//! requests), so it reaches a fixed point, and the result does not
//! depend on visit order. Unit generation then walks the active
//! `(package, domain)` states in sorted order, so unit ids are stable for
//! a given input.
//!
//! ### Dev-dependencies
//!
//! [`resolve`] runs the resolver up to twice, as `cargo build` and
//! `cargo test` would. The build pass ignores dev edges and yields the
//! libs, bins and build scripts. When a root package has
//! `harness = false` tests, a test pass also follows the dev edges of
//! those roots, so dev-dependency features unify with the normal ones,
//! and yields the tests and what they link. A test-pass unit equal to a
//! build unit merges into it; the others are marked
//! [`Unit::test_only`]. Only workspace members keep their
//! dev-dependencies (see [`super::workspace`]), so only theirs apply.
//!
//! ### Host tests
//!
//! Packages listed in `q.host_tests()` join the test pass as roots in
//! [`Domain::HostTest`], with their dev edges. That domain has the
//! host's cfgs but its own feature sets: Cargo does not unify the
//! features of normal dependencies with those of build-dependencies and
//! proc macros, and `Domain::Host` holds the latter. Each such root
//! yields a [`UnitKind::LibTest`] (its lib built with `--test`), a
//! [`UnitKind::BinTest`] per bin (the bin built with `--test`), and a
//! [`UnitKind::HarnessTest`] per `harness = true` test in `tests/`.
//! Its units are not root units: they take none of a group's settings.
//! A target with `test = false` yields no unit.
//!
//! A host-test package given features (`package_with_features`)
//! resolves in a pass of its own instead, as `cargo test -p <name>
//! --features ..` would, so its features reach no other host test. Its
//! units join the graph the way test-pass units do: one equal to a unit
//! already there merges into it.

use super::platform::TargetPlatform;
use super::workspace::{
    DepKind, DepTarget, FeatureValue, Package, PackageIdx, PackageSet, PackageSource,
    sort_diagnostics,
};
use crate::error::{Diagnostic, Error, Result};
use cargo_platform::Cfg;
use quon_model::SourceSpan;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

/// A feature unification domain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Domain {
    Host,
    /// The host, for the unit tests of `q.host_tests()` packages and
    /// what they link. See the module docs.
    HostTest,
    /// A Quon target, by name.
    Target(String),
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Domain::Host => f.write_str("host"),
            Domain::HostTest => f.write_str("host-test"),
            Domain::Target(t) => f.write_str(t),
        }
    }
}

pub type UnitId = usize;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnitKind {
    Lib,
    ProcMacro,
    Bin(String),
    /// An integration test with `harness = false`, built like a bin.
    Test(String),
    /// The lib of a host-test root, built with `--test`.
    LibTest,
    /// A bin of a host-test root, built with `--test` for its unit
    /// tests.
    BinTest(String),
    /// A `harness = true` test of a host-test root, built with `--test`.
    HarnessTest(String),
    BuildScript,
}

impl UnitKind {
    /// Whether only `quon test` runs the unit.
    pub fn is_test(&self) -> bool {
        matches!(
            self,
            UnitKind::Test(_) | UnitKind::LibTest | UnitKind::BinTest(_) | UnitKind::HarnessTest(_)
        )
    }
}

impl fmt::Display for UnitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnitKind::Lib => f.write_str("lib"),
            UnitKind::ProcMacro => f.write_str("proc-macro"),
            UnitKind::Bin(n) => write!(f, "bin {n}"),
            UnitKind::Test(n) => write!(f, "test {n}"),
            UnitKind::LibTest => f.write_str("lib-test"),
            UnitKind::BinTest(n) => write!(f, "bin-test {n}"),
            UnitKind::HarnessTest(n) => write!(f, "harness-test {n}"),
            UnitKind::BuildScript => f.write_str("build-script"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnitDep {
    pub extern_name: String,
    pub unit: UnitId,
    pub kind: DepKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildScriptRun {
    /// The [`UnitKind::BuildScript`] unit to run. Always in
    /// [`Domain::Host`].
    pub script: UnitId,
    /// Env vars known at resolution time.
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub id: UnitId,
    pub package: PackageIdx,
    pub kind: UnitKind,
    pub domain: Domain,
    pub features: BTreeSet<String>,
    /// Sorted by extern name.
    pub deps: Vec<UnitDep>,
    pub unit_hash: String,
    /// Set on lib, proc-macro and bin units of a package with a build
    /// script.
    pub build_script: Option<BuildScriptRun>,
    /// Lib, bin and test units of a root package in the root's domain.
    pub is_root: bool,
    /// Only `quon test` needs the unit: a test, or a test-pass unit no
    /// build unit equals (a dev-dependency, or a copy whose features or
    /// deps grew). See the module docs.
    pub test_only: bool,
    /// On a [`UnitKind::LibTest`]: the package's plain lib unit in the
    /// same domain, which its doctests link. Not a dep, since the lib
    /// test is that lib, built with `--test`. `None` when the lib says
    /// `doctest = false`.
    pub doctest_lib: Option<UnitId>,
    /// On a [`UnitKind::HarnessTest`]: the package's bin units in the
    /// same domain, whose paths the test gets as `CARGO_BIN_EXE_<name>`,
    /// as under Cargo. Not deps: a test does not link them.
    pub bins: Vec<UnitId>,
}

#[derive(Debug, Clone)]
pub struct UnitGraph {
    /// Indexed by [`UnitId`]. Sorted by (package id, domain, kind,
    /// features, test-only, unit hash).
    pub units: Vec<Unit>,
    pub packages: Vec<Package>,
    /// The cfg set per domain, in rustc's order.
    pub cfgs: BTreeMap<Domain, Vec<Cfg>>,
    pub host_triple: String,
}

impl UnitGraph {
    /// Every unit in [`Domain::Host`].
    pub fn host_units(&self) -> impl Iterator<Item = &Unit> {
        self.units.iter().filter(|u| u.domain == Domain::Host)
    }

    /// One unique name per unit, indexed by [`UnitId`].
    ///
    /// A unit is named after its package (lib, proc-macro), its bin or
    /// test, or `<package>@build-script`. When names collide, root units
    /// keep the plain name and the others get suffixes in this order:
    /// `<bin>@bin-test` for a bin built with `--test`, then `@test` for
    /// a test sharing a name with any other unit,
    /// `@host-test` for a [`Domain::HostTest`] unit sharing a name with a
    /// unit of another domain,
    /// `@test-pass` for another test-only unit sharing a name with a unit
    /// the build uses, `@lib` for
    /// a lib sharing a name with a bin of its package, `@<domain>` for a
    /// copy outside the group's primary domain (the root's domain, else
    /// the first target, else host), `@<version>` for all but the newest
    /// version. A last `#<n>` suffix separates build-script units that
    /// differ only in features.
    pub fn unit_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .units
            .iter()
            .map(|u| {
                let pkg = &self.packages[u.package];
                match &u.kind {
                    UnitKind::Bin(n) | UnitKind::Test(n) | UnitKind::HarnessTest(n) => n.clone(),
                    // Its own base name: a package's lib test and the
                    // test of its same-named bin would otherwise collide.
                    UnitKind::BinTest(n) => format!("{n}@bin-test"),
                    UnitKind::BuildScript => format!("{}@build-script", pkg.name()),
                    _ => pkg.name().to_string(),
                }
            })
            .collect();

        type Step<'a> = Box<dyn Fn(&[UnitId], &UnitGraph) -> Vec<(UnitId, String)> + 'a>;
        let steps: Vec<Step> = vec![
            // A test never takes the plain name from a bin or lib, so
            // adding `tests/kernel.rs` does not rename the `kernel` bin.
            Box::new(|group, g| {
                group
                    .iter()
                    .filter(|&&i| g.units[i].kind.is_test())
                    .map(|&i| (i, "@test".to_string()))
                    .collect()
            }),
            // Host-test copies always take the same suffix, whether or
            // not the build or a target's tests use the package too.
            Box::new(|group, g| {
                if group.iter().all(|&i| g.units[i].domain == Domain::HostTest) {
                    return Vec::new();
                }
                group
                    .iter()
                    .filter(|&&i| g.units[i].domain == Domain::HostTest)
                    .map(|&i| (i, "@host-test".to_string()))
                    .collect()
            }),
            // Likewise a test-only copy of a unit, so the build's names,
            // and with them its artifacts, do not depend on the tests.
            // Once renamed, no group mixes build and test-only units.
            Box::new(|group, g| {
                if group.iter().all(|&i| g.units[i].test_only) {
                    return Vec::new();
                }
                group
                    .iter()
                    .filter(|&&i| g.units[i].test_only)
                    .map(|&i| (i, "@test-pass".to_string()))
                    .collect()
            }),
            Box::new(|group, g| {
                let has_bin = |p| {
                    group.iter().any(|&i| {
                        g.units[i].package == p && matches!(g.units[i].kind, UnitKind::Bin(_))
                    })
                };
                group
                    .iter()
                    .filter(|&&i| g.units[i].kind == UnitKind::Lib && has_bin(g.units[i].package))
                    .map(|&i| (i, "@lib".to_string()))
                    .collect()
            }),
            Box::new(|group, g| {
                let primary = group
                    .iter()
                    .find(|&&i| g.units[i].is_root)
                    .map(|&i| g.units[i].domain.clone())
                    .or_else(|| {
                        group
                            .iter()
                            .map(|&i| &g.units[i].domain)
                            .filter(|d| matches!(d, Domain::Target(_)))
                            .min()
                            .cloned()
                    })
                    .unwrap_or(Domain::Host);
                group
                    .iter()
                    .filter(|&&i| !g.units[i].is_root && g.units[i].domain != primary)
                    .map(|&i| (i, format!("@{}", g.units[i].domain)))
                    .collect()
            }),
            Box::new(|group, g| {
                let newest = group
                    .iter()
                    .map(|&i| &g.packages[g.units[i].package].id.version)
                    .max()
                    .cloned();
                group
                    .iter()
                    .filter(|&&i| {
                        !g.units[i].is_root
                            && Some(&g.packages[g.units[i].package].id.version) != newest.as_ref()
                    })
                    .map(|&i| (i, format!("@{}", g.packages[g.units[i].package].id.version)))
                    .collect()
            }),
            Box::new(|group, g| {
                group
                    .iter()
                    .filter(|&&i| !g.units[i].is_root)
                    .enumerate()
                    .skip(1)
                    .map(|(n, &i)| (i, format!("#{n}")))
                    .collect()
            }),
        ];
        for step in steps {
            let mut groups: BTreeMap<String, Vec<UnitId>> = BTreeMap::new();
            for (i, n) in names.iter().enumerate() {
                groups.entry(n.clone()).or_default().push(i);
            }
            for group in groups.values().filter(|g| g.len() > 1) {
                for (i, suffix) in step(group, self) {
                    names[i].push_str(&suffix);
                }
            }
        }
        names
    }
}

/// A `q.host_tests()` package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTestRoot {
    pub package: PackageIdx,
    /// `Some` for a package that resolves on its own, with these
    /// features. See the module docs.
    pub features: Option<Vec<String>>,
}

/// A package to build in a domain, like `cargo build -p`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub package: PackageIdx,
    pub domain: Domain,
    pub features: Vec<String>,
    pub default_features: bool,
}

/// Resolve features for `roots`, and for `host_tests` in the test pass.
/// `platforms` must hold every root domain and [`Domain::Host`], and
/// [`Domain::HostTest`] when `host_tests` is not empty.
///
/// The build pass never sees dev edges, so the kernel's units, and so
/// its artifacts, are the same whether or not tests or dev-dependencies
/// are declared. The cost: a dependency whose features a dev-dependency
/// grows builds twice, once per pass. One unified pass would build it
/// once but let test-only features into the kernel.
pub fn resolve(
    set: &PackageSet,
    roots: &[Root],
    host_tests: &[HostTestRoot],
    platforms: &BTreeMap<Domain, TargetPlatform>,
    host_triple: &str,
) -> Result<UnitGraph> {
    let states = run_resolver(set, roots, platforms, BTreeSet::new())?;
    let mut units = build_units(set, roots, &states, platforms, host_triple, Pass::Build)?;
    let mut test_roots: BTreeSet<Key> = roots
        .iter()
        .filter(|r| {
            set.packages[r.package]
                .tests
                .iter()
                .any(|t| t.test && !t.harness)
        })
        .map(|r| (r.package, r.domain.clone()))
        .collect();
    // The build pass never sees the host-test roots, so `quon build`
    // builds none of their units.
    let mut all_roots = roots.to_vec();
    let mut isolated = Vec::new();
    for h in host_tests {
        let root = Root {
            package: h.package,
            domain: Domain::HostTest,
            features: h.features.clone().unwrap_or_default(),
            default_features: true,
        };
        if h.features.is_some() {
            isolated.push(root);
        } else {
            test_roots.insert((h.package, Domain::HostTest));
            all_roots.push(root);
        }
    }
    if !test_roots.is_empty() {
        let states = run_resolver(set, &all_roots, platforms, test_roots)?;
        let test = build_units(set, &all_roots, &states, platforms, host_triple, Pass::Test)?;
        units = merge_test_units(units, test);
    }
    // Merging by hash means a dependency these passes give the same
    // features, and so the same deps, builds once across all of them.
    for root in isolated {
        let dev_roots = BTreeSet::from([(root.package, Domain::HostTest)]);
        let roots = [root];
        let states = run_resolver(set, &roots, platforms, dev_roots)?;
        let test = build_units(set, &roots, &states, platforms, host_triple, Pass::Test)?;
        units = merge_test_units(units, test);
    }
    Ok(UnitGraph {
        units,
        packages: set.packages.clone(),
        cfgs: platforms
            .iter()
            .map(|(d, p)| (d.clone(), p.cfgs.clone()))
            .collect(),
        host_triple: host_triple.to_string(),
    })
}

/// Run the resolver for `roots`, following the dev edges of the states
/// in `dev_roots`.
fn run_resolver(
    set: &PackageSet,
    roots: &[Root],
    platforms: &BTreeMap<Domain, TargetPlatform>,
    dev_roots: BTreeSet<Key>,
) -> Result<BTreeMap<Key, State>> {
    let mut r = Resolver {
        set,
        platforms,
        dev_roots,
        states: BTreeMap::new(),
        queue: VecDeque::new(),
        errors: Vec::new(),
    };
    for root in roots {
        let pkg = &set.packages[root.package];
        if root.domain != Domain::Host && pkg.is_proc_macro() {
            r.errors.push(Diagnostic::error(format!(
                "proc-macro package `{}` cannot be a root of target `{}`",
                pkg.name(),
                root.domain
            )));
            continue;
        }
        let d = r.domain_for(root.package, &root.domain);
        r.activate_package(root.package, &d, None);
        if root.default_features && pkg.features.contains_key("default") {
            r.request(
                root.package,
                &d,
                FeatureValue::Feature("default".into()),
                None,
            );
        }
        for f in &root.features {
            r.request(root.package, &d, FeatureValue::parse(f), None);
        }
    }
    r.run();
    if !r.errors.is_empty() {
        sort_diagnostics(&mut r.errors);
        r.errors
            .dedup_by(|a, b| a.message == b.message && a.span == b.span);
        return Err(Error::Diagnostics(r.errors));
    }
    Ok(r.states)
}

type Key = (PackageIdx, Domain);

#[derive(Debug, Default)]
struct State {
    features: BTreeSet<String>,
    /// Indices into `Package::deps` of followed edges.
    edges: BTreeSet<usize>,
    /// Stored `x?/f` requests: (dep name, feature).
    weak: BTreeSet<(String, String)>,
    /// The edge that first activated this state, for error chains.
    via: Option<(Key, usize)>,
}

struct Request {
    key: Key,
    value: FeatureValue,
    span: Option<SourceSpan>,
}

struct Resolver<'a> {
    set: &'a PackageSet,
    platforms: &'a BTreeMap<Domain, TargetPlatform>,
    /// States whose dev edges apply: in the test pass, the roots with
    /// tests.
    dev_roots: BTreeSet<Key>,
    states: BTreeMap<Key, State>,
    queue: VecDeque<Request>,
    errors: Vec<Diagnostic>,
}

impl Resolver<'_> {
    fn pkg(&self, p: PackageIdx) -> &Package {
        &self.set.packages[p]
    }

    /// Proc macros always build for the host.
    fn domain_for(&self, p: PackageIdx, d: &Domain) -> Domain {
        if self.pkg(p).is_proc_macro() {
            Domain::Host
        } else {
            d.clone()
        }
    }

    fn request(
        &mut self,
        p: PackageIdx,
        d: &Domain,
        value: FeatureValue,
        span: Option<SourceSpan>,
    ) {
        self.queue.push_back(Request {
            key: (p, d.clone()),
            value,
            span,
        });
    }

    /// Whether edge `i` of `p` applies in domain `d`: dev edges only from
    /// a state in `dev_roots`, build edges only when `p` has a build
    /// script (Cargo ignores them otherwise), and the platform key
    /// matches the domain the edge is evaluated in (the host for build
    /// edges, `d` for normal and dev edges).
    fn edge_applies(&self, p: PackageIdx, d: &Domain, i: usize) -> bool {
        let pkg = self.pkg(p);
        let dep = &pkg.deps[i];
        let eval = match dep.kind {
            DepKind::Dev if !self.dev_roots.contains(&(p, d.clone())) => return false,
            DepKind::Dev => d,
            DepKind::Build if pkg.build_script.is_none() => return false,
            DepKind::Build => &Domain::Host,
            DepKind::Normal => d,
        };
        match &dep.platform {
            None => true,
            Some(pl) => self.platforms.get(eval).is_some_and(|tp| tp.matches(pl)),
        }
    }

    fn activate_package(&mut self, p: PackageIdx, d: &Domain, via: Option<(Key, usize)>) {
        let key = (p, d.clone());
        if self.states.contains_key(&key) {
            return;
        }
        self.states.insert(
            key,
            State {
                via,
                ..Default::default()
            },
        );
        for i in 0..self.pkg(p).deps.len() {
            if !self.pkg(p).deps[i].optional && self.edge_applies(p, d, i) {
                self.follow_edge(p, d, i);
            }
        }
    }

    /// Report an activated edge that has no package.
    fn report_unresolved(&mut self, p: PackageIdx, d: &Domain, i: usize) {
        let dep = &self.pkg(p).deps[i];
        let DepTarget::Unresolved(reason) = &dep.target else {
            return;
        };
        let what = match dep.kind {
            DepKind::Dev => "dev-dependency",
            DepKind::Normal | DepKind::Build => "dependency",
        };
        let diag = Diagnostic::error(format!(
            "{what} `{}` of `{}`: {reason}",
            dep.name,
            self.pkg(p).name()
        ))
        .with_optional_span(dep.span.clone());
        let diag = self.with_chain(diag, &(p, d.clone()));
        self.errors.push(diag);
    }

    fn follow_edge(&mut self, p: PackageIdx, d: &Domain, i: usize) {
        let key = (p, d.clone());
        let new = self
            .states
            .get_mut(&key)
            .expect("edge followed from an active state")
            .edges
            .insert(i);
        if !new {
            return;
        }
        let Some((c, cd)) = self.child_key(p, d, i) else {
            self.report_unresolved(p, d, i);
            return;
        };
        self.activate_package(c, &cd, Some((key.clone(), i)));
        let dep = self.pkg(p).deps[i].clone();
        if dep.default_features && self.pkg(c).features.contains_key("default") {
            self.request(
                c,
                &cd,
                FeatureValue::Feature("default".into()),
                dep.span.clone(),
            );
        }
        for f in &dep.features {
            self.request(c, &cd, FeatureValue::parse(f), dep.span.clone());
        }
        let weak: Vec<String> = self.states[&key]
            .weak
            .iter()
            .filter(|(name, _)| *name == dep.name)
            .map(|(_, f)| f.clone())
            .collect();
        for f in weak {
            self.request(c, &cd, FeatureValue::Feature(f), dep.span.clone());
        }
    }

    fn run(&mut self) {
        while let Some(req) = self.queue.pop_front() {
            self.process(req);
        }
    }

    fn process(&mut self, req: Request) {
        let (p, d) = req.key.clone();
        match req.value {
            FeatureValue::Feature(f) => {
                let Some(entries) = self.pkg(p).features.get(&f).cloned() else {
                    let pkg = self.pkg(p);
                    let mut diag =
                        Diagnostic::error(format!("package `{}` has no feature `{f}`", pkg.name()))
                            .with_optional_span(req.span);
                    diag = self.with_chain(diag, &req.key);
                    let pkg = self.pkg(p);
                    diag = if pkg.features.is_empty() {
                        diag.with_note(format!("{} defines no features", pkg.name()))
                    } else {
                        let names: Vec<&str> = pkg.features.keys().map(|s| s.as_str()).collect();
                        diag.with_note(format!("{} defines {}", pkg.name(), names.join(", ")))
                    };
                    self.errors.push(diag);
                    return;
                };
                let new = self
                    .states
                    .get_mut(&req.key)
                    .expect("features requested on an active state")
                    .features
                    .insert(f);
                if new {
                    for e in entries {
                        self.request(p, &d, e.value, e.span);
                    }
                }
            }
            FeatureValue::Dep(x) => {
                for i in self.edges_named(p, &d, &x) {
                    if self.pkg(p).deps[i].optional {
                        self.follow_edge(p, &d, i);
                    }
                }
            }
            FeatureValue::DepFeature { dep, feature, weak } => {
                if !self.pkg(p).deps.iter().any(|x| x.name == dep) {
                    let diag = Diagnostic::error(format!(
                        "feature `{dep}/{feature}` on `{}`: `{dep}` is not a dependency",
                        self.pkg(p).name()
                    ))
                    .with_optional_span(req.span);
                    let diag = self.with_chain(diag, &req.key);
                    self.errors.push(diag);
                    return;
                }
                for i in self.edges_named(p, &d, &dep) {
                    let optional = self.pkg(p).deps[i].optional;
                    let active = self.states[&req.key].edges.contains(&i);
                    if optional && weak && !active {
                        self.states
                            .get_mut(&req.key)
                            .expect("active state")
                            .weak
                            .insert((dep.clone(), feature.clone()));
                        continue;
                    }
                    if optional && !weak && self.pkg(p).features.contains_key(&dep) {
                        // `x/f` on an optional `x` also enables the
                        // feature named `x`, as Cargo does.
                        self.request(p, &d, FeatureValue::Feature(dep.clone()), req.span.clone());
                    }
                    self.follow_edge(p, &d, i);
                    if let Some((c, cd)) = self.child_key(p, &d, i) {
                        self.request(
                            c,
                            &cd,
                            FeatureValue::Feature(feature.clone()),
                            req.span.clone(),
                        );
                    }
                }
            }
        }
    }

    /// The child of edge `i`: in `Host` for build edges and proc macros,
    /// else in `d`. `None` for an unresolved edge.
    fn child_key(&self, p: PackageIdx, d: &Domain, i: usize) -> Option<Key> {
        let dep = &self.pkg(p).deps[i];
        match dep.target {
            DepTarget::Resolved(c) if dep.kind == DepKind::Build => Some((c, Domain::Host)),
            DepTarget::Resolved(c) => Some((c, self.domain_for(c, d))),
            DepTarget::Unresolved(_) => None,
        }
    }

    fn edges_named(&self, p: PackageIdx, d: &Domain, name: &str) -> Vec<usize> {
        (0..self.pkg(p).deps.len())
            .filter(|&i| self.pkg(p).deps[i].name == name && self.edge_applies(p, d, i))
            .collect()
    }

    /// Add "required by a -> b -> c in <domain>" from the activation
    /// chain of `key`.
    fn with_chain(&self, diag: Diagnostic, key: &Key) -> Diagnostic {
        let mut chain = vec![self.pkg(key.0).name().to_string()];
        let mut cur = key.clone();
        while let Some((parent, _)) = self.states.get(&cur).and_then(|s| s.via.clone()) {
            chain.push(self.pkg(parent.0).name().to_string());
            cur = parent;
            if chain.len() > self.set.packages.len() {
                break;
            }
        }
        if chain.len() == 1 {
            return diag;
        }
        chain.reverse();
        let place = match &key.1 {
            Domain::Host => "the host".to_string(),
            Domain::HostTest => "the host tests".to_string(),
            Domain::Target(t) => format!("target {t}"),
        };
        diag.with_note(format!("required by {} in {place}", chain.join(" -> ")))
    }
}

/// Which resolver pass units come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Libs, bins and build scripts, without dev edges.
    Build,
    /// Libs, tests and build scripts, with the roots' dev edges, plus
    /// the host-test roots.
    Test,
}

/// The units of one pass, hashed, with ids local to the pass.
fn build_units(
    set: &PackageSet,
    roots: &[Root],
    states: &BTreeMap<Key, State>,
    platforms: &BTreeMap<Domain, TargetPlatform>,
    host_triple: &str,
    pass: Pass,
) -> Result<Vec<Unit>> {
    let root_keys: BTreeSet<Key> = roots
        .iter()
        .map(|r| (r.package, r.domain.clone()))
        .collect();

    // Draft units, keyed by the state they come from.
    struct Draft {
        package: PackageIdx,
        kind: UnitKind,
        domain: Domain,
        features: BTreeSet<String>,
        /// The state whose edges become deps.
        state: Key,
        is_root: bool,
    }
    let mut drafts = Vec::new();
    for (key, st) in states {
        let pkg = &set.packages[key.0];
        // Host-test roots are not root units: they join no group. See
        // the module docs.
        let host_test_root = key.1 == Domain::HostTest && root_keys.contains(key);
        let is_root = !host_test_root && root_keys.contains(key);
        if let Some(lib) = &pkg.lib {
            drafts.push(Draft {
                package: key.0,
                kind: if lib.proc_macro {
                    UnitKind::ProcMacro
                } else {
                    UnitKind::Lib
                },
                domain: key.1.clone(),
                features: st.features.clone(),
                state: key.clone(),
                is_root,
            });
        }
        if is_root && pass == Pass::Build {
            for b in &pkg.bins {
                if b.required_features.iter().all(|f| st.features.contains(f)) {
                    drafts.push(Draft {
                        package: key.0,
                        kind: UnitKind::Bin(b.name.clone()),
                        domain: key.1.clone(),
                        features: st.features.clone(),
                        state: key.clone(),
                        is_root,
                    });
                }
            }
        }
        if is_root && pass == Pass::Test {
            // Tests that need libtest are left to `cargo test`;
            // expansion warns about them.
            for t in pkg.tests.iter().filter(|t| t.test && !t.harness) {
                if t.required_features.iter().all(|f| st.features.contains(f)) {
                    drafts.push(Draft {
                        package: key.0,
                        kind: UnitKind::Test(t.name.clone()),
                        domain: key.1.clone(),
                        features: st.features.clone(),
                        state: key.clone(),
                        is_root,
                    });
                }
            }
        }
        if host_test_root {
            let draft = |kind| Draft {
                package: key.0,
                kind,
                domain: key.1.clone(),
                features: st.features.clone(),
                state: key.clone(),
                is_root: false,
            };
            // `test = false` leaves a target out, as `cargo test` does:
            // a firmware bin that only builds for its MCU says so.
            if pkg.lib.as_ref().is_some_and(|l| !l.proc_macro && l.test) {
                drafts.push(draft(UnitKind::LibTest));
            }
            for b in pkg.bins.iter().filter(|b| b.test) {
                if b.required_features.iter().all(|f| st.features.contains(f)) {
                    drafts.push(draft(UnitKind::BinTest(b.name.clone())));
                }
            }
            // `harness = false` tests are QEMU tests; they build only
            // for the package's target roots.
            let mut harness_tests = false;
            for t in pkg.tests.iter().filter(|t| t.test && t.harness) {
                if t.required_features.iter().all(|f| st.features.contains(f)) {
                    drafts.push(draft(UnitKind::HarnessTest(t.name.clone())));
                    harness_tests = true;
                }
            }
            // As Cargo does, a package's tests in `tests/` get its bins
            // built, to run them through `CARGO_BIN_EXE_<name>`.
            if harness_tests {
                for b in &pkg.bins {
                    if b.required_features.iter().all(|f| st.features.contains(f)) {
                        drafts.push(draft(UnitKind::Bin(b.name.clone())));
                    }
                }
            }
        }
        if pkg.build_script.is_some() {
            drafts.push(Draft {
                package: key.0,
                kind: UnitKind::BuildScript,
                domain: Domain::Host,
                features: st.features.clone(),
                state: key.clone(),
                is_root: false,
            });
        }
    }
    drafts.sort_by(|a, b| {
        (a.package, &a.domain, &a.kind, &a.features, &a.state).cmp(&(
            b.package,
            &b.domain,
            &b.kind,
            &b.features,
            &b.state,
        ))
    });
    // Build-script units with equal features in equal packages are one
    // unit (one compile serves both runs), as in Cargo.
    drafts.dedup_by(|a, b| {
        a.kind == UnitKind::BuildScript
            && b.kind == UnitKind::BuildScript
            && a.package == b.package
            && a.features == b.features
    });

    let lib_unit: BTreeMap<Key, UnitId> = drafts
        .iter()
        .enumerate()
        .filter(|(_, d)| matches!(d.kind, UnitKind::Lib | UnitKind::ProcMacro))
        .map(|(i, d)| ((d.package, d.domain.clone()), i))
        .collect();
    let script_unit = |p: PackageIdx, features: &BTreeSet<String>| {
        drafts.iter().position(|d| {
            d.kind == UnitKind::BuildScript && d.package == p && &d.features == features
        })
    };

    let mut units = Vec::with_capacity(drafts.len());
    for (id, d) in drafts.iter().enumerate() {
        let pkg = &set.packages[d.package];
        let st = &states[&d.state];
        // Only tests link dev-dependencies, as in Cargo, so a
        // dev-dependency that depends on the package's lib is no cycle.
        let wants = |k: DepKind| match d.kind {
            UnitKind::BuildScript => k == DepKind::Build,
            ref kind if kind.is_test() => k != DepKind::Build,
            _ => k == DepKind::Normal,
        };
        let mut deps = BTreeSet::new();
        for &i in &st.edges {
            let dep = &pkg.deps[i];
            if !wants(dep.kind) {
                continue;
            }
            let DepTarget::Resolved(c) = dep.target else {
                continue;
            };
            let cd = if dep.kind == DepKind::Build || set.packages[c].is_proc_macro() {
                Domain::Host
            } else {
                d.state.1.clone()
            };
            // A dependency without a lib target has nothing to link;
            // Cargo ignores it with a warning.
            let (Some(&unit), Some(lib)) = (lib_unit.get(&(c, cd)), &set.packages[c].lib) else {
                continue;
            };
            let extern_name = if dep.name != dep.package {
                dep.name.replace('-', "_")
            } else {
                lib.name.replace('-', "_")
            };
            deps.insert(UnitDep {
                extern_name,
                unit,
                kind: dep.kind,
            });
        }
        if let (
            UnitKind::Bin(_) | UnitKind::Test(_) | UnitKind::BinTest(_) | UnitKind::HarnessTest(_),
            Some(lib),
        ) = (&d.kind, &pkg.lib)
            && let Some(&unit) = lib_unit.get(&(d.package, d.domain.clone()))
        {
            deps.insert(UnitDep {
                extern_name: lib.name.replace('-', "_"),
                unit,
                kind: DepKind::Normal,
            });
        }
        let build_script = match d.kind {
            UnitKind::BuildScript => None,
            _ if pkg.build_script.is_some() => {
                script_unit(d.package, &d.features).map(|script| BuildScriptRun {
                    script,
                    env: build_script_env(pkg, &d.features, &d.state.1, platforms, host_triple),
                })
            }
            _ => None,
        };
        units.push(Unit {
            id,
            package: d.package,
            kind: d.kind.clone(),
            domain: d.domain.clone(),
            features: d.features.clone(),
            deps: deps.into_iter().collect(),
            unit_hash: String::new(),
            build_script,
            is_root: d.is_root,
            test_only: d.kind.is_test(),
            doctest_lib: match (&d.kind, &pkg.lib) {
                (UnitKind::LibTest, Some(lib)) if lib.doctest => {
                    lib_unit.get(&(d.package, d.domain.clone())).copied()
                }
                _ => None,
            },
            bins: match d.kind {
                UnitKind::HarnessTest(_) => drafts
                    .iter()
                    .enumerate()
                    .filter(|(_, o)| {
                        matches!(o.kind, UnitKind::Bin(_))
                            && o.package == d.package
                            && o.domain == d.domain
                    })
                    .map(|(i, _)| i)
                    .collect(),
                _ => Vec::new(),
            },
        });
    }
    for u in &mut units {
        sort_deps(&mut u.deps);
    }
    compute_hashes(set, &mut units)?;
    Ok(units)
}

/// Sort by extern name and drop repeats of one extern and unit: a
/// test's dev-dependency on its own package names the lib the test
/// already links.
fn sort_deps(deps: &mut Vec<UnitDep>) {
    deps.sort_by(|a, b| (&a.extern_name, a.unit).cmp(&(&b.extern_name, b.unit)));
    deps.dedup_by(|a, b| a.extern_name == b.extern_name && a.unit == b.unit);
}

/// Add to the build pass's units the tests and every test-pass unit
/// they reach that no build unit equals. `build` may already hold the
/// units of earlier test passes, which then count as build units. Equal means equal hash, which
/// covers package, kind, domain, features and the deps' hashes, so a
/// dependency the dev edges leave alone builds once. Ids follow the
/// sort order, which keeps the build units in their relative order.
fn merge_test_units(build: Vec<Unit>, test: Vec<Unit>) -> Vec<Unit> {
    let in_build: BTreeMap<&str, UnitId> =
        build.iter().map(|u| (u.unit_hash.as_str(), u.id)).collect();
    // The deps of a unit equal to a build unit are equal to build units
    // too, so the walk stops there.
    let mut needed = BTreeSet::new();
    let mut stack: Vec<UnitId> = test
        .iter()
        .filter(|u| u.kind.is_test())
        .map(|u| u.id)
        .collect();
    while let Some(id) = stack.pop() {
        let u = &test[id];
        if in_build.contains_key(u.unit_hash.as_str()) || !needed.insert(id) {
            continue;
        }
        stack.extend(u.deps.iter().map(|d| d.unit));
        stack.extend(u.build_script.as_ref().map(|b| b.script));
        // Nothing else links the plain lib when the package has no
        // `tests/` files, and nothing links the bins.
        stack.extend(u.doctest_lib);
        stack.extend(u.bins.iter().copied());
    }

    let mut merged: Vec<(bool, &Unit)> = build.iter().map(|u| (false, u)).collect();
    merged.extend(
        test.iter()
            .filter(|u| needed.contains(&u.id))
            .map(|u| (true, u)),
    );
    merged.sort_by(|&(at, a), &(bt, b)| {
        (a.package, &a.domain, &a.kind, &a.features, at, &a.unit_hash).cmp(&(
            b.package,
            &b.domain,
            &b.kind,
            &b.features,
            bt,
            &b.unit_hash,
        ))
    });

    let mut build_ids = vec![0; build.len()];
    let mut test_ids = vec![0; test.len()];
    for (new, &(from_test, u)) in merged.iter().enumerate() {
        if from_test {
            test_ids[u.id] = new;
        } else {
            build_ids[u.id] = new;
        }
    }
    // A test-pass unit left out above equals a build unit: point at it.
    for u in test.iter().filter(|u| !needed.contains(&u.id)) {
        if let Some(&b) = in_build.get(u.unit_hash.as_str()) {
            test_ids[u.id] = build_ids[b];
        }
    }

    merged
        .iter()
        .enumerate()
        .map(|(new, &(from_test, u))| {
            let ids = if from_test { &test_ids } else { &build_ids };
            let mut u = u.clone();
            u.id = new;
            u.test_only |= from_test;
            for d in &mut u.deps {
                d.unit = ids[d.unit];
            }
            sort_deps(&mut u.deps);
            if let Some(b) = &mut u.build_script {
                b.script = ids[b.script];
            }
            u.doctest_lib = u.doctest_lib.map(|l| ids[l]);
            for b in &mut u.bins {
                *b = ids[*b];
            }
            u
        })
        .collect()
}

/// SHA-256 over the package id, unit kind, domain, sorted features and
/// the sorted hashes of the unit's deps. Path packages hash their path
/// relative to the workspace root, so the hash does not depend on where
/// the checkout lives.
fn compute_hashes(set: &PackageSet, units: &mut [Unit]) -> Result<()> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        None,
        Visiting,
        Done,
    }
    fn visit(
        id: UnitId,
        set: &PackageSet,
        units: &mut [Unit],
        marks: &mut [Mark],
    ) -> Result<String> {
        match marks[id] {
            Mark::Done => return Ok(units[id].unit_hash.clone()),
            Mark::Visiting => {
                return Err(Error::Diagnostics(vec![Diagnostic::error(format!(
                    "dependency cycle through package `{}`",
                    set.packages[units[id].package].name()
                ))]));
            }
            Mark::None => {}
        }
        marks[id] = Mark::Visiting;
        let dep_ids: Vec<UnitId> = units[id].deps.iter().map(|d| d.unit).collect();
        let mut dep_hashes = Vec::new();
        for d in dep_ids {
            dep_hashes.push(visit(d, set, units, marks)?);
        }
        dep_hashes.sort();
        let u = &units[id];
        let pkg = &set.packages[u.package];
        let mut h = Sha256::new();
        h.update(b"quon.unit.v1\0");
        h.update(pkg.id.name.as_bytes());
        h.update(b"\0");
        h.update(pkg.id.version.to_string().as_bytes());
        h.update(b"\0");
        match &pkg.id.source {
            PackageSource::Path(dir) => {
                let rel = dir.strip_prefix(&set.root).unwrap_or(dir);
                h.update(b"path:");
                h.update(rel.as_os_str().as_encoded_bytes());
            }
            PackageSource::Registry => h.update(b"registry"),
            PackageSource::Git(source) => {
                h.update(b"git:");
                h.update(source.as_bytes());
            }
        }
        h.update(b"\0");
        h.update(u.kind.to_string().as_bytes());
        h.update(b"\0");
        h.update(u.domain.to_string().as_bytes());
        for f in &u.features {
            h.update(b"\0f:");
            h.update(f.as_bytes());
        }
        for d in &dep_hashes {
            h.update(b"\0d:");
            h.update(d.as_bytes());
        }
        let hash: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        units[id].unit_hash = hash.clone();
        marks[id] = Mark::Done;
        Ok(hash)
    }
    let mut marks = vec![Mark::None; units.len()];
    for id in 0..units.len() {
        visit(id, set, units, &mut marks)?;
    }
    Ok(())
}

/// `CARGO_PKG_*`, `CARGO_MANIFEST_DIR` and `CARGO_MANIFEST_PATH` for a
/// package, as Cargo sets them for compilation and build scripts. Cargo
/// sets every variable, empty when the manifest has no value.
pub fn package_env(pkg: &Package) -> BTreeMap<String, String> {
    let v = &pkg.id.version;
    let m = &pkg.metadata;
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    let mut env = BTreeMap::new();
    let mut set = |k: &str, val: String| {
        env.insert(k.to_string(), val);
    };
    set("CARGO_PKG_NAME", pkg.id.name.clone());
    set("CARGO_PKG_VERSION", v.to_string());
    set("CARGO_PKG_VERSION_MAJOR", v.major.to_string());
    set("CARGO_PKG_VERSION_MINOR", v.minor.to_string());
    set("CARGO_PKG_VERSION_PATCH", v.patch.to_string());
    set("CARGO_PKG_VERSION_PRE", v.pre.to_string());
    set("CARGO_PKG_AUTHORS", m.authors.join(":"));
    set("CARGO_PKG_DESCRIPTION", opt(&m.description));
    set("CARGO_PKG_HOMEPAGE", opt(&m.homepage));
    set("CARGO_PKG_REPOSITORY", opt(&m.repository));
    set("CARGO_PKG_LICENSE", opt(&m.license));
    set("CARGO_PKG_LICENSE_FILE", opt(&m.license_file));
    set("CARGO_PKG_README", opt(&m.readme));
    set("CARGO_PKG_RUST_VERSION", opt(&m.rust_version));
    set(
        "CARGO_MANIFEST_DIR",
        pkg.manifest_dir().display().to_string(),
    );
    set(
        "CARGO_MANIFEST_PATH",
        pkg.manifest_path.display().to_string(),
    );
    env
}

/// The build-script env known at resolution time for `pkg` running for
/// domain `d` with `features`.
fn build_script_env(
    pkg: &Package,
    features: &BTreeSet<String>,
    d: &Domain,
    platforms: &BTreeMap<Domain, TargetPlatform>,
    host_triple: &str,
) -> BTreeMap<String, String> {
    let mut env = package_env(pkg);
    for f in features {
        env.insert(
            format!("CARGO_FEATURE_{}", f.to_uppercase().replace('-', "_")),
            "1".into(),
        );
    }
    if let Some(platform) = platforms.get(d) {
        // Values joined with `,` in rustc's order; name-only cfgs are
        // present with an empty value.
        let mut cfgs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for cfg in &platform.cfgs {
            let (key, value) = match cfg {
                Cfg::Name(n) => (n.as_str().to_string(), None),
                Cfg::KeyPair(k, v) => (k.as_str().to_string(), Some(v.clone())),
            };
            if key == "feature" || key == "debug_assertions" {
                continue;
            }
            let values = cfgs.entry(key).or_default();
            values.extend(value);
        }
        for (key, values) in cfgs {
            env.insert(
                format!("CARGO_CFG_{}", key.to_uppercase()),
                values.join(","),
            );
        }
        env.insert("TARGET".into(), platform.name.clone());
    }
    env.insert("HOST".into(), host_triple.to_string());
    env
}

/// Render the graph as stable text, one line per unit: name, domain,
/// kind, features, `extern -> unit` edges and the build script to run.
pub fn dump_units(g: &UnitGraph) -> String {
    let names = g.unit_names();
    let mut out = String::new();
    for u in &g.units {
        let features: Vec<&str> = u.features.iter().map(|s| s.as_str()).collect();
        let deps: Vec<String> = u
            .deps
            .iter()
            .map(|d| format!("{} -> {}", d.extern_name, names[d.unit]))
            .collect();
        out.push_str(&format!(
            "{} ({}, {}) features=[{}] deps=[{}]",
            names[u.id],
            u.domain,
            u.kind,
            features.join(","),
            deps.join(", ")
        ));
        if let Some(bs) = &u.build_script {
            out.push_str(&format!(" runs={}", names[bs.script]));
        }
        out.push('\n');
    }
    out
}
