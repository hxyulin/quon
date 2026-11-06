//! Workspace discovery, manifest inheritance and the package set.
//!
//! [`load_workspace`] reads the workspace root manifest, expands its
//! members, applies `[workspace.package]`, `[workspace.dependencies]`
//! and `[workspace.lints]` inheritance, and then follows every
//! dependency edge to the package it names:
//!
//! - path dependencies load the manifest at the path;
//! - registry dependencies are mapped through `Cargo.lock` to an exact
//!   version, whose source directory comes from a [`RegistrySources`]
//!   map supplied by the caller.
//!
//! A registry edge that cannot be mapped (no `Cargo.lock`, no lock
//! entry, missing source directory) does not fail the load. The edge
//! is recorded as [`DepTarget::Unresolved`] with the reason, and the
//! feature resolver reports it only if it activates the edge. Cargo
//! leaves optional dependencies that no feature enables out of
//! `Cargo.lock` and `vendor/`, so failing eagerly would reject valid
//! workspaces.
//!
//! The output is ordered by package id and member name, never by
//! filesystem or hash order, so the same tree always loads the same.

use super::lock::Lockfile;
use super::schema::{
    DepTable, DetailedDependency, Inheritable, TomlBuild, TomlDependency, TomlLib, TomlLint,
    TomlManifest, TomlReadme, TomlWorkspace, TomlWorkspacePackage,
};
use super::sources::RegistrySources;
use crate::error::{Diagnostic, Error, Result};
use cargo_platform::Platform;
use gluon_model::SourceSpan;
use semver::{Version, VersionReq};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use toml::Spanned;

pub type PackageIdx = usize;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PackageSource {
    /// A path package; the absolute manifest directory.
    Path(PathBuf),
    /// A crates.io package, read from Cargo's registry cache or a
    /// vendor directory.
    Registry,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageId {
    pub name: String,
    pub version: Version,
    pub source: PackageSource,
}

impl std::fmt::Display for PackageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.source {
            PackageSource::Path(dir) => {
                write!(f, "{} {} ({})", self.name, self.version, dir.display())
            }
            PackageSource::Registry => write!(f, "{} {} (registry)", self.name, self.version),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepKind {
    Normal,
    Build,
    Dev,
}

impl DepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DepKind::Normal => "normal",
            DepKind::Build => "build",
            DepKind::Dev => "dev",
        }
    }
}

/// Package metadata that becomes `CARGO_PKG_*` env vars.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageMetadata {
    pub authors: Vec<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    pub license_file: Option<String>,
    pub readme: Option<String>,
    pub rust_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibTarget {
    /// The rustc crate name: `[lib] name`, else the package name with
    /// `-` replaced by `_`.
    pub name: String,
    /// Absolute path of the crate root.
    pub path: PathBuf,
    pub edition: String,
    pub proc_macro: bool,
    /// `crate-type` as written; `["lib"]` when absent.
    pub crate_types: Vec<String>,
    pub test: bool,
    pub doctest: bool,
    pub bench: bool,
    pub harness: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinTarget {
    pub name: String,
    pub path: PathBuf,
    pub edition: String,
    pub required_features: Vec<String>,
}

/// An integration test: a `[[test]]` table or a file Cargo would find
/// under `tests/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestTarget {
    pub name: String,
    pub path: PathBuf,
    pub edition: String,
    pub required_features: Vec<String>,
    /// `harness`, true when absent. Gluon builds only `harness = false`
    /// tests, since a harness needs libtest; lowering rejects the rest.
    pub harness: bool,
}

/// One value in a `[features]` list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FeatureValue {
    /// `"f"`: another feature of this package.
    Feature(String),
    /// `"dep:x"`: activate optional dependency `x`.
    Dep(String),
    /// `"x/f"` (`weak = false`) or `"x?/f"` (`weak = true`).
    DepFeature {
        dep: String,
        feature: String,
        weak: bool,
    },
}

impl FeatureValue {
    pub fn parse(s: &str) -> Self {
        if let Some(dep) = s.strip_prefix("dep:") {
            return FeatureValue::Dep(dep.to_string());
        }
        if let Some((dep, feature)) = s.split_once('/') {
            let (dep, weak) = match dep.strip_suffix('?') {
                Some(d) => (d, true),
                None => (dep, false),
            };
            return FeatureValue::DepFeature {
                dep: dep.to_string(),
                feature: feature.to_string(),
                weak,
            };
        }
        FeatureValue::Feature(s.to_string())
    }
}

impl std::fmt::Display for FeatureValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeatureValue::Feature(n) => f.write_str(n),
            FeatureValue::Dep(d) => write!(f, "dep:{d}"),
            FeatureValue::DepFeature { dep, feature, weak } => {
                write!(f, "{dep}{}/{feature}", if *weak { "?" } else { "" })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureEntry {
    pub value: FeatureValue,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepSource {
    /// Absolute directory of the dependency's manifest, plus an optional
    /// version requirement that the path package must satisfy.
    Path(PathBuf, Option<VersionReq>),
    Registry(VersionReq),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepTarget {
    Resolved(PackageIdx),
    /// The reason the edge could not be mapped to a package. Reported
    /// only if the resolver activates the edge.
    Unresolved(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The key in the dependency table. Differs from `package` for a
    /// renamed dependency.
    pub name: String,
    /// The package name the dependency refers to.
    pub package: String,
    pub kind: DepKind,
    /// `[target.'...'.dependencies]` key, if any.
    pub platform: Option<Platform>,
    pub optional: bool,
    pub default_features: bool,
    pub features: Vec<String>,
    pub source: DepSource,
    pub target: DepTarget,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lint {
    pub level: String,
    pub priority: i8,
    pub check_cfg: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub id: PackageId,
    pub manifest_path: PathBuf,
    pub is_member: bool,
    pub edition: String,
    pub metadata: PackageMetadata,
    pub lib: Option<LibTarget>,
    pub bins: Vec<BinTarget>,
    /// Integration tests. Empty for non-members: nothing builds a
    /// dependency's tests.
    pub tests: Vec<TestTarget>,
    /// Absolute path of the build script, if the package has one.
    pub build_script: Option<PathBuf>,
    pub links: Option<String>,
    /// Explicit features plus one implicit `x = ["dep:x"]` per optional
    /// dependency `x` that no feature names as `dep:x`.
    pub features: BTreeMap<String, Vec<FeatureEntry>>,
    pub deps: Vec<Dependency>,
    /// tool → lint → level. Empty for non-members, whose lints Cargo
    /// caps anyway.
    pub lints: BTreeMap<String, BTreeMap<String, Lint>>,
}

impl Package {
    pub fn name(&self) -> &str {
        &self.id.name
    }

    pub fn manifest_dir(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(Path::new(""))
    }

    pub fn is_proc_macro(&self) -> bool {
        self.lib.as_ref().is_some_and(|l| l.proc_macro)
    }
}

#[derive(Debug, Clone)]
pub struct PackageSet {
    /// Absolute, canonical workspace root.
    pub root: PathBuf,
    /// Sorted by [`PackageId`].
    pub packages: Vec<Package>,
    /// Workspace members, sorted by name.
    pub members: Vec<PackageIdx>,
    pub lock: Option<Lockfile>,
    pub warnings: Vec<Diagnostic>,
}

impl PackageSet {
    pub fn member(&self, name: &str) -> Option<PackageIdx> {
        self.members
            .iter()
            .copied()
            .find(|&i| self.packages[i].name() == name)
    }
}

/// Load the workspace whose root manifest is `<root>/Cargo.toml`.
/// Registry package sources are looked up in `sources`.
pub fn load_workspace(root: &Path, sources: &RegistrySources) -> Result<PackageSet> {
    let mut loader = Loader::new(root, sources)?;
    loader.load()?;
    loader.finish()
}

/// The nearest parent directory whose `Cargo.toml` has a `[workspace]`
/// table that does not exclude `dir`, which is where Cargo would look
/// for the workspace of a package in `dir`.
fn enclosing_workspace(dir: &Path) -> Result<Option<PathBuf>> {
    for parent in dir.ancestors().skip(1) {
        let path = parent.join("Cargo.toml");
        if !path.is_file() {
            continue;
        }
        let file = SourceFile::read(&path)?;
        let manifest = file.parse().map_err(|d| Error::Diagnostics(vec![d]))?;
        if let Some(ws) = manifest.workspace {
            let excluded = ws.exclude.iter().any(|e| dir.starts_with(parent.join(e)));
            return Ok((!excluded).then(|| parent.to_path_buf()));
        }
    }
    Ok(None)
}

/// Source text of one manifest, for turning byte ranges into positions.
struct SourceFile {
    path: PathBuf,
    text: String,
}

impl SourceFile {
    fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            text,
        })
    }

    fn span(&self, range: std::ops::Range<usize>) -> SourceSpan {
        let pos = |off: usize| {
            let before = &self.text[..off.min(self.text.len())];
            let line = before.matches('\n').count() as u32 + 1;
            let col = before.rsplit('\n').next().unwrap_or("").chars().count() as u32 + 1;
            (line, col)
        };
        SourceSpan::range(&self.path, pos(range.start), pos(range.end))
    }

    fn parse(&self) -> std::result::Result<TomlManifest, Diagnostic> {
        toml::from_str(&self.text).map_err(|e| {
            let d = Diagnostic::error(format!("invalid manifest: {}", e.message()));
            match e.span() {
                Some(r) => d.with_span(self.span(r)),
                None => d.with_note(format!("in {}", self.path.display())),
            }
        })
    }
}

/// What a member may inherit from the workspace root.
struct WorkspaceCtx {
    root_file: SourceFile,
    package: TomlWorkspacePackage,
    dependencies: DepTable,
    lints: BTreeMap<String, BTreeMap<String, TomlLint>>,
}

struct Loader<'s> {
    root: PathBuf,
    sources: &'s RegistrySources,
    ws: WorkspaceCtx,
    excluded: Vec<PathBuf>,
    lock: Option<Lockfile>,
    lock_error: Option<String>,
    packages: Vec<Package>,
    by_id: BTreeMap<PackageId, PackageIdx>,
    /// Packages whose dependencies are not yet mapped to packages.
    pending: VecDeque<(PackageIdx, Vec<Dependency>)>,
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
}

impl<'s> Loader<'s> {
    fn new(root: &Path, sources: &'s RegistrySources) -> Result<Self> {
        let root = std::fs::canonicalize(root).map_err(|e| Error::Io {
            path: root.to_path_buf(),
            source: e,
        })?;
        let root_file = SourceFile::read(&root.join("Cargo.toml"))?;
        let manifest = root_file.parse().map_err(|d| Error::Diagnostics(vec![d]))?;
        // A package without a `[workspace]` table is, to Cargo, a workspace
        // whose only member is that package, unless a parent directory
        // holds a workspace. Then Cargo either builds it as part of that
        // workspace (with that workspace's lockfile and inherited fields)
        // or refuses, so loading it alone would build different code.
        let lone = TomlWorkspace::default();
        let ws = match (&manifest.workspace, &manifest.package) {
            (Some(ws), _) => ws,
            (None, Some(_)) => {
                if let Some(parent) = enclosing_workspace(&root)? {
                    return Err(Error::Diagnostics(vec![
                        Diagnostic::error(
                            "this package is inside a Cargo workspace; point Gluon at the \
                             workspace root",
                        )
                        .with_note(format!("in {}", root_file.path.display()))
                        .with_note(format!("workspace root: {}", parent.display())),
                    ]));
                }
                &lone
            }
            (None, None) => {
                return Err(Error::Diagnostics(vec![
                    Diagnostic::error("manifest has neither `[package]` nor `[workspace]`")
                        .with_note(format!("in {}", root_file.path.display())),
                ]));
            }
        };
        let excluded = ws.exclude.iter().map(|e| root.join(e)).collect();
        let mut errors = Vec::new();
        if manifest.patch.is_some() || manifest.replace.is_some() {
            errors.push(
                Diagnostic::error(
                    "`[patch]` and `[replace]` are not supported: Gluon would build \
                     different code than Cargo",
                )
                .with_note(format!("in {}", root_file.path.display())),
            );
        }
        let root_edition = manifest.package.as_ref().and_then(|p| match &p.edition {
            Some(Inheritable::Value(e)) => Some(e.clone()),
            Some(Inheritable::Workspace { .. }) => {
                ws.package.as_ref().and_then(|wp| wp.edition.clone())
            }
            None => None,
        });
        match ws.resolver.as_deref() {
            Some("2" | "3") => {}
            Some(other) => errors.push(
                Diagnostic::error(format!(
                    "resolver `{other}` is not supported; Gluon implements resolver 2"
                ))
                .with_note(format!("in {}", root_file.path.display())),
            ),
            None if root_edition.as_deref().is_some_and(|e| e >= "2021") => {}
            None => errors.push(
                Diagnostic::error(
                    "workspace defaults to resolver 1, which Gluon does not support; \
                     set `resolver = \"2\"` in `[workspace]`",
                )
                .with_note(format!("in {}", root_file.path.display())),
            ),
        }
        let ws_ctx = WorkspaceCtx {
            package: ws.package.clone().unwrap_or_default(),
            dependencies: ws.dependencies.clone(),
            lints: ws.lints.clone().unwrap_or_default(),
            root_file,
        };

        let lock_path = root.join("Cargo.lock");
        let (lock, lock_error) = if lock_path.exists() {
            (Some(Lockfile::load(&lock_path)?), None)
        } else {
            (
                None,
                Some(format!(
                    "{} is missing; {}",
                    lock_path.display(),
                    sources.lock_hint()
                )),
            )
        };

        Ok(Self {
            root,
            sources,
            ws: ws_ctx,
            excluded,
            lock,
            lock_error,
            packages: Vec::new(),
            by_id: BTreeMap::new(),
            pending: VecDeque::new(),
            errors,
            warnings: Vec::new(),
        })
    }

    fn is_member_dir(&self, dir: &Path) -> bool {
        dir.starts_with(&self.root) && !self.excluded.iter().any(|e| dir.starts_with(e))
    }

    fn load(&mut self) -> Result<()> {
        let mut member_dirs = BTreeSet::new();
        let root_manifest = self
            .ws
            .root_file
            .parse()
            .map_err(|d| Error::Diagnostics(vec![d]))?;
        if root_manifest.package.is_some() {
            member_dirs.insert(self.root.clone());
        }
        let patterns = root_manifest
            .workspace
            .as_ref()
            .map(|w| w.members.clone())
            .unwrap_or_default();
        for pattern in &patterns {
            let full = self.root.join(pattern);
            let full = full.to_string_lossy();
            let paths =
                match glob::glob(&full) {
                    Ok(p) => p,
                    Err(e) => {
                        self.errors.push(self.root_error(format!(
                            "invalid workspace member pattern `{pattern}`: {e}"
                        )));
                        continue;
                    }
                };
            let mut matched: Vec<PathBuf> = paths.filter_map(|p| p.ok()).collect();
            matched.sort();
            for dir in matched {
                if !dir.is_dir() || self.excluded.iter().any(|e| dir.starts_with(e)) {
                    continue;
                }
                if !dir.join("Cargo.toml").is_file() {
                    self.errors.push(self.root_error(format!(
                        "workspace member `{}` (from pattern `{pattern}`) has no Cargo.toml",
                        dir.display()
                    )));
                    continue;
                }
                match std::fs::canonicalize(&dir) {
                    Ok(d) => {
                        member_dirs.insert(d);
                    }
                    Err(e) => self.errors.push(self.root_error(format!(
                        "cannot read workspace member {}: {e}",
                        dir.display()
                    ))),
                }
            }
        }

        for dir in member_dirs {
            self.load_path_package(&dir, true);
        }

        let mut names: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for p in &self.packages {
            names
                .entry(p.id.name.clone())
                .or_default()
                .push(p.manifest_path.clone());
        }
        for (name, paths) in names.iter().filter(|(_, p)| p.len() > 1) {
            let mut d = self.root_error(format!("two workspace members are named `{name}`"));
            for p in paths {
                d = d.with_note(format!("{}", p.display()));
            }
            self.errors.push(d);
        }

        while let Some((idx, mut deps)) = self.pending.pop_front() {
            for dep in &mut deps {
                dep.target = self.map_dependency(idx, dep);
            }
            self.packages[idx].deps = deps;
        }
        Ok(())
    }

    fn root_error(&self, msg: String) -> Diagnostic {
        Diagnostic::error(msg).with_note(format!("in {}", self.ws.root_file.path.display()))
    }

    /// Load (or find) the path package in `dir`. Returns its index, or
    /// `None` after recording an error.
    fn load_path_package(&mut self, dir: &Path, listed_member: bool) -> Option<PackageIdx> {
        if let Some((_, &idx)) = self
            .by_id
            .iter()
            .find(|(id, _)| id.source == PackageSource::Path(dir.to_path_buf()))
        {
            return Some(idx);
        }
        let is_member = listed_member || self.is_member_dir(dir);
        let file = match SourceFile::read(&dir.join("Cargo.toml")) {
            Ok(f) => f,
            Err(e) => {
                self.errors.push(Diagnostic::error(e.to_string()));
                return None;
            }
        };
        let source = PackageSource::Path(dir.to_path_buf());
        self.add_package(file, source, is_member)
    }

    fn add_package(
        &mut self,
        file: SourceFile,
        source: PackageSource,
        is_member: bool,
    ) -> Option<PackageIdx> {
        let manifest = match file.parse() {
            Ok(m) => m,
            Err(d) => {
                self.errors.push(d);
                return None;
            }
        };
        let mut cx = ManifestCx {
            file: &file,
            ws: is_member.then_some(&self.ws),
            errors: Vec::new(),
            warnings: Vec::new(),
        };
        let lowered = cx.lower(&manifest, source, is_member);
        let (errors, warnings) = (cx.errors, cx.warnings);
        self.errors.extend(errors);
        self.warnings.extend(warnings);
        let (package, deps) = lowered?;
        if let Some(&idx) = self.by_id.get(&package.id) {
            return Some(idx);
        }
        let idx = self.packages.len();
        self.by_id.insert(package.id.clone(), idx);
        self.packages.push(package);
        self.pending.push_back((idx, deps));
        Some(idx)
    }

    fn map_dependency(&mut self, parent: PackageIdx, dep: &Dependency) -> DepTarget {
        match &dep.source {
            DepSource::Path(dir, req) => {
                let Some(idx) = self.load_path_package(dir, false) else {
                    return DepTarget::Unresolved(format!(
                        "path dependency {} failed to load",
                        dir.display()
                    ));
                };
                let found = &self.packages[idx].id;
                if found.name != dep.package {
                    self.errors.push(
                        Diagnostic::error(format!(
                            "path dependency `{}` points at package `{}`; add \
                             `package = \"{}\"` to rename it",
                            dep.name, found.name, found.name
                        ))
                        .with_optional_span(dep.span.clone()),
                    );
                } else if let Some(req) = req
                    && !req.matches(&found.version)
                {
                    self.errors.push(
                        Diagnostic::error(format!(
                            "path dependency `{}` requires version `{req}` but {} is {}",
                            dep.name,
                            dir.display(),
                            found.version
                        ))
                        .with_optional_span(dep.span.clone()),
                    );
                }
                DepTarget::Resolved(idx)
            }
            DepSource::Registry(req) => match self.map_registry(parent, dep, req) {
                Ok(t) => t,
                Err(reason) => DepTarget::Unresolved(reason),
            },
        }
    }

    fn map_registry(
        &mut self,
        parent: PackageIdx,
        dep: &Dependency,
        req: &VersionReq,
    ) -> std::result::Result<DepTarget, String> {
        let Some(lock) = &self.lock else {
            return Err(self.lock_error.clone().unwrap_or_default());
        };
        let pid = &self.packages[parent].id;
        let parent_entry = match &pid.source {
            PackageSource::Path(_) => lock.path_package(&pid.name, &pid.version),
            PackageSource::Registry => lock.registry_package(&pid.name, &pid.version),
        }
        .ok_or_else(|| {
            format!(
                "Cargo.lock has no entry for `{} {}`; {}",
                pid.name,
                pid.version,
                self.sources.lock_hint()
            )
        })?;
        let locked = lock
            .locked_registry_dep(parent_entry, &dep.package, req)
            .map_err(|m| format!("{m}; {}", self.sources.lock_hint()))?;
        let id = PackageId {
            name: locked.name.clone(),
            version: locked.version.clone(),
            source: PackageSource::Registry,
        };
        if let Some(&idx) = self.by_id.get(&id) {
            return Ok(DepTarget::Resolved(idx));
        }
        let manifest = self.sources.dir(&id.name, &id.version)?.join("Cargo.toml");
        let file = SourceFile::read(&manifest).map_err(|e| e.to_string())?;
        match self.add_package(file, PackageSource::Registry, false) {
            Some(idx) if self.packages[idx].id == id => Ok(DepTarget::Resolved(idx)),
            Some(idx) => Err(format!(
                "{} holds `{} {}`, expected `{} {}`",
                manifest.display(),
                self.packages[idx].id.name,
                self.packages[idx].id.version,
                id.name,
                id.version
            )),
            None => Err(format!("{} failed to load", manifest.display())),
        }
    }

    /// Sort packages by id and remap every index.
    fn finish(mut self) -> Result<PackageSet> {
        if !self.errors.is_empty() {
            sort_diagnostics(&mut self.errors);
            return Err(Error::Diagnostics(self.errors));
        }
        let mut order: Vec<PackageIdx> = (0..self.packages.len()).collect();
        order.sort_by(|&a, &b| self.packages[a].id.cmp(&self.packages[b].id));
        let mut new_index = vec![0; order.len()];
        for (new, &old) in order.iter().enumerate() {
            new_index[old] = new;
        }
        let mut slots: Vec<Option<Package>> = self.packages.into_iter().map(Some).collect();
        let mut packages: Vec<Package> = order
            .iter()
            .map(|&old| slots[old].take().expect("each index taken once"))
            .collect();
        for p in &mut packages {
            for d in &mut p.deps {
                if let DepTarget::Resolved(i) = &mut d.target {
                    *i = new_index[*i];
                }
            }
        }
        let mut members: Vec<PackageIdx> = (0..packages.len())
            .filter(|&i| packages[i].is_member)
            .collect();
        members.sort_by(|&a, &b| packages[a].id.name.cmp(&packages[b].id.name));
        sort_diagnostics(&mut self.warnings);
        Ok(PackageSet {
            root: self.root,
            packages,
            members,
            lock: self.lock,
            warnings: self.warnings,
        })
    }
}

pub(crate) fn sort_diagnostics(diags: &mut [Diagnostic]) {
    diags.sort_by(|a, b| {
        let key = |d: &Diagnostic| {
            (
                d.span
                    .as_ref()
                    .map(|s| (s.file.clone(), s.line, s.col))
                    .unwrap_or_default(),
                d.message.clone(),
            )
        };
        key(a).cmp(&key(b))
    });
}

/// Lowers one parsed manifest into a [`Package`] plus its unmapped
/// dependencies, collecting diagnostics.
struct ManifestCx<'a> {
    file: &'a SourceFile,
    /// Present for workspace members, which may inherit.
    ws: Option<&'a WorkspaceCtx>,
    errors: Vec<Diagnostic>,
    warnings: Vec<Diagnostic>,
}

impl ManifestCx<'_> {
    fn error(&mut self, msg: impl Into<String>, span: Option<SourceSpan>) {
        let d = Diagnostic::error(msg);
        let d = match span {
            Some(s) => d.with_span(s),
            None => d.with_note(format!("in {}", self.file.path.display())),
        };
        self.errors.push(d);
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings
            .push(Diagnostic::warning(msg).with_note(format!("in {}", self.file.path.display())));
    }

    fn inherit<T: Clone>(
        &mut self,
        field: &str,
        value: &Option<Inheritable<T>>,
        ws_value: impl Fn(&TomlWorkspacePackage) -> Option<T>,
    ) -> Option<T> {
        match value {
            None => None,
            Some(Inheritable::Value(v)) => Some(v.clone()),
            Some(Inheritable::Workspace { workspace: false }) => {
                self.error(format!("`{field}.workspace` may only be `true`"), None);
                None
            }
            Some(Inheritable::Workspace { workspace: true }) => {
                let Some(ws) = self.ws else {
                    self.error(
                        format!(
                            "`{field}.workspace = true` in a package outside the workspace \
                             is not supported"
                        ),
                        None,
                    );
                    return None;
                };
                let v = ws_value(&ws.package);
                if v.is_none() {
                    self.error(
                        format!(
                            "`{field}.workspace = true`, but `[workspace.package]` has no `{field}`"
                        ),
                        None,
                    );
                }
                v
            }
        }
    }

    fn lower(
        &mut self,
        m: &TomlManifest,
        source: PackageSource,
        is_member: bool,
    ) -> Option<(Package, Vec<Dependency>)> {
        let Some(pkg) = &m.package else {
            self.error("manifest has no `[package]` table", None);
            return None;
        };
        let dir = self
            .file
            .path
            .parent()
            .unwrap_or(Path::new(""))
            .to_path_buf();
        let version_str = self
            .inherit("version", &pkg.version, |w| w.version.clone())
            .unwrap_or_else(|| "0.0.0".into());
        let version = match Version::parse(&version_str) {
            Ok(v) => v,
            Err(e) => {
                self.error(format!("invalid version `{version_str}`: {e}"), None);
                return None;
            }
        };
        let edition = self
            .inherit("edition", &pkg.edition, |w| w.edition.clone())
            .unwrap_or_else(|| "2015".into());
        self.check_edition(&edition);

        let readme = match self.inherit("readme", &pkg.readme, |w| w.readme.clone()) {
            Some(TomlReadme::Path(p)) => Some(p),
            Some(TomlReadme::Flag(false)) => None,
            Some(TomlReadme::Flag(true)) => Some("README.md".into()),
            // Cargo's default: the first of these that exists.
            None => ["README.md", "README.txt", "README"]
                .into_iter()
                .find(|f| dir.join(f).is_file())
                .map(str::to_owned),
        };
        let metadata = PackageMetadata {
            authors: self
                .inherit("authors", &pkg.authors, |w| w.authors.clone())
                .unwrap_or_default(),
            description: self.inherit("description", &pkg.description, |w| w.description.clone()),
            homepage: self.inherit("homepage", &pkg.homepage, |w| w.homepage.clone()),
            repository: self.inherit("repository", &pkg.repository, |w| w.repository.clone()),
            license: self.inherit("license", &pkg.license, |w| w.license.clone()),
            license_file: self.inherit("license-file", &pkg.license_file, |w| {
                w.license_file.clone()
            }),
            readme,
            rust_version: self.inherit("rust-version", &pkg.rust_version, |w| {
                w.rust_version.clone()
            }),
        };

        if m.profile.is_some() && is_member {
            self.warn("`[profile.*]` tables are ignored; Gluon profiles apply");
        }
        let lib = self.lower_lib(m, &pkg.name, &dir, &edition, pkg.autolib);
        let bins = self.lower_bins(m, &pkg.name, &dir, &edition, pkg.autobins);
        let tests = if is_member {
            self.lower_tests(m, &dir, &edition, pkg.autotests)
        } else {
            Vec::new()
        };
        let build_script = match &pkg.build {
            Some(TomlBuild::Path(p)) => Some(dir.join(p)),
            Some(TomlBuild::Flag(false)) => None,
            Some(TomlBuild::Flag(true)) => Some(dir.join("build.rs")),
            None => Some(dir.join("build.rs")).filter(|p| p.is_file()),
        };

        let mut deps = Vec::new();
        let tables = [
            (DepKind::Normal, &m.dependencies),
            (DepKind::Build, &m.build_dependencies),
            (DepKind::Dev, &m.dev_dependencies),
        ];
        for (kind, table) in tables {
            self.lower_dep_table(table, kind, None, &dir, &edition, is_member, &mut deps);
        }
        for (key, t) in &m.target {
            let span = [&t.dependencies, &t.build_dependencies, &t.dev_dependencies]
                .into_iter()
                .flat_map(|table| table.values())
                .map(|e| self.file.span(e.span()))
                .min_by_key(|s| (s.line, s.col));
            let platform = match Platform::from_str(key) {
                Ok(p) => p,
                Err(e) => {
                    self.error(format!("invalid target key `{key}`: {e}"), span);
                    continue;
                }
            };
            if let Platform::Cfg(expr) = &platform
                && expr.to_string().contains("debug_assertions")
            {
                self.warnings.push(
                    Diagnostic::warning(format!(
                        "`{key}` never matches: `debug_assertions` is not set for dependency \
                         selection"
                    ))
                    .with_optional_span(span),
                );
            }
            let tables = [
                (DepKind::Normal, &t.dependencies),
                (DepKind::Build, &t.build_dependencies),
                (DepKind::Dev, &t.dev_dependencies),
            ];
            for (kind, table) in tables {
                self.lower_dep_table(
                    table,
                    kind,
                    Some(&platform),
                    &dir,
                    &edition,
                    is_member,
                    &mut deps,
                );
            }
        }

        // A feature may name a dev-dependency (`syn` has `test =
        // ["syn-test-suite/all-features"]`). A non-member's dev-deps are
        // dropped above, so their names are kept for validation.
        let dropped_dev: BTreeSet<&str> = if is_member {
            BTreeSet::new()
        } else {
            std::iter::once(&m.dev_dependencies)
                .chain(m.target.values().map(|t| &t.dev_dependencies))
                .flat_map(|table| table.keys())
                .map(String::as_str)
                .collect()
        };
        let features = self.lower_features(m, &deps, &dropped_dev);
        let lints = if is_member {
            self.lower_lints(m)
        } else {
            BTreeMap::new()
        };

        let package = Package {
            id: PackageId {
                name: pkg.name.clone(),
                version,
                source,
            },
            manifest_path: self.file.path.clone(),
            is_member,
            edition,
            metadata,
            lib,
            bins,
            tests,
            build_script,
            links: pkg.links.clone(),
            features,
            deps: Vec::new(),
            lints,
        };
        Some((package, deps))
    }

    fn check_edition(&mut self, edition: &str) {
        if !matches!(edition, "2015" | "2018" | "2021" | "2024") {
            self.error(format!("unsupported edition `{edition}`"), None);
        }
    }

    fn lower_lib(
        &mut self,
        m: &TomlManifest,
        pkg_name: &str,
        dir: &Path,
        edition: &str,
        autolib: Option<bool>,
    ) -> Option<LibTarget> {
        let implicit = dir.join("src/lib.rs");
        let default_lib = TomlLib::default();
        let lib = match &m.lib {
            Some(lib) => lib,
            None if autolib != Some(false) && implicit.is_file() => &default_lib,
            None => return None,
        };
        let path = match &lib.path {
            Some(p) => dir.join(p),
            None => implicit,
        };
        if !path.is_file() {
            self.error(
                format!("lib target source {} does not exist", path.display()),
                None,
            );
        }
        let edition = lib.edition.clone().unwrap_or_else(|| edition.to_string());
        self.check_edition(&edition);
        Some(LibTarget {
            name: lib
                .name
                .clone()
                .unwrap_or_else(|| pkg_name.replace('-', "_")),
            path,
            edition,
            proc_macro: lib.proc_macro.unwrap_or(false),
            crate_types: lib.crate_type.clone().unwrap_or_else(|| vec!["lib".into()]),
            test: lib.test.unwrap_or(true),
            doctest: lib.doctest.unwrap_or(true),
            bench: lib.bench.unwrap_or(true),
            harness: lib.harness.unwrap_or(true),
        })
    }

    fn lower_bins(
        &mut self,
        m: &TomlManifest,
        pkg_name: &str,
        dir: &Path,
        edition: &str,
        autobins: Option<bool>,
    ) -> Vec<BinTarget> {
        let mut bins = Vec::new();
        for b in &m.bin {
            let path = match &b.path {
                Some(p) => Some(dir.join(p)),
                None => {
                    let candidates = [
                        (b.name == pkg_name).then(|| dir.join("src/main.rs")),
                        Some(dir.join(format!("src/bin/{}.rs", b.name))),
                        Some(dir.join(format!("src/bin/{}/main.rs", b.name))),
                    ];
                    candidates.into_iter().flatten().find(|p| p.is_file())
                }
            };
            let Some(path) = path else {
                self.error(format!("cannot find the source of bin `{}`", b.name), None);
                continue;
            };
            let edition = b.edition.clone().unwrap_or_else(|| edition.to_string());
            self.check_edition(&edition);
            bins.push(BinTarget {
                name: b.name.clone(),
                path,
                edition,
                required_features: b.required_features.clone(),
            });
        }
        if autobins != Some(false) {
            let mut inferred = Vec::new();
            let main = dir.join("src/main.rs");
            if main.is_file() {
                inferred.push((pkg_name.to_string(), main));
            }
            if let Ok(rd) = std::fs::read_dir(dir.join("src/bin")) {
                let mut entries: Vec<PathBuf> =
                    rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                entries.sort();
                for p in entries {
                    if p.extension().is_some_and(|e| e == "rs") && p.is_file() {
                        let stem = p
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        inferred.push((stem, p));
                    } else if p.join("main.rs").is_file() {
                        let name = p
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        inferred.push((name, p.join("main.rs")));
                    }
                }
            }
            for (name, path) in inferred {
                if bins.iter().any(|b| b.name == name || b.path == path) {
                    continue;
                }
                bins.push(BinTarget {
                    name,
                    path,
                    edition: edition.to_string(),
                    required_features: Vec::new(),
                });
            }
        }
        bins.sort_by(|a, b| a.name.cmp(&b.name));
        bins
    }

    /// `[[test]]` tables, then, unless `autotests = false`, each
    /// `tests/<name>.rs` and `tests/<name>/main.rs` no table already
    /// names, sorted by name. The same rules as Cargo's target discovery.
    fn lower_tests(
        &mut self,
        m: &TomlManifest,
        dir: &Path,
        edition: &str,
        autotests: Option<bool>,
    ) -> Vec<TestTarget> {
        let mut tests = Vec::new();
        for t in &m.test {
            let path = match &t.path {
                Some(p) => Some(dir.join(p)),
                None => [
                    dir.join(format!("tests/{}.rs", t.name)),
                    dir.join(format!("tests/{}/main.rs", t.name)),
                ]
                .into_iter()
                .find(|p| p.is_file()),
            };
            let Some(path) = path else {
                self.error(format!("cannot find the source of test `{}`", t.name), None);
                continue;
            };
            let edition = t.edition.clone().unwrap_or_else(|| edition.to_string());
            self.check_edition(&edition);
            tests.push(TestTarget {
                name: t.name.clone(),
                path,
                edition,
                required_features: t.required_features.clone(),
                harness: t.harness.unwrap_or(true),
            });
        }
        if autotests != Some(false)
            && let Ok(rd) = std::fs::read_dir(dir.join("tests"))
        {
            let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            entries.sort();
            for p in entries {
                let (name, path) = if p.extension().is_some_and(|e| e == "rs") && p.is_file() {
                    (p.file_stem().unwrap_or_default(), p.clone())
                } else if p.join("main.rs").is_file() {
                    (p.file_name().unwrap_or_default(), p.join("main.rs"))
                } else {
                    continue;
                };
                let name = name.to_string_lossy().into_owned();
                if tests.iter().any(|t| t.name == name || t.path == path) {
                    continue;
                }
                tests.push(TestTarget {
                    name,
                    path,
                    edition: edition.to_string(),
                    required_features: Vec::new(),
                    harness: true,
                });
            }
        }
        tests.sort_by(|a, b| a.name.cmp(&b.name));
        tests
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_dep_table(
        &mut self,
        table: &DepTable,
        kind: DepKind,
        platform: Option<&Platform>,
        dir: &Path,
        edition: &str,
        is_member: bool,
        out: &mut Vec<Dependency>,
    ) {
        // Dev-dependencies of non-members never take part in a build,
        // and Cargo leaves them out of `Cargo.lock`.
        if kind == DepKind::Dev && !is_member {
            return;
        }
        for (key, entry) in table {
            let span = self.file.span(entry.span());
            if let Some(dep) =
                self.lower_dep(key, entry.get_ref(), span, kind, platform, dir, edition)
            {
                out.push(dep);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_dep(
        &mut self,
        key: &str,
        entry: &TomlDependency,
        span: SourceSpan,
        kind: DepKind,
        platform: Option<&Platform>,
        dir: &Path,
        edition: &str,
    ) -> Option<Dependency> {
        let detailed = match entry {
            TomlDependency::Simple(v) => DetailedDependency {
                version: Some(v.clone()),
                ..Default::default()
            },
            TomlDependency::Detailed(d) => d.clone(),
        };
        if detailed.workspace == Some(true) {
            return self.lower_inherited_dep(key, &detailed, span, kind, platform, edition);
        }
        if detailed.workspace == Some(false) {
            self.error(
                "`workspace = false` is not valid on a dependency",
                Some(span.clone()),
            );
        }
        self.lower_plain_dep(key, &detailed, span, kind, platform, dir)
    }

    fn lower_plain_dep(
        &mut self,
        key: &str,
        d: &DetailedDependency,
        span: SourceSpan,
        kind: DepKind,
        platform: Option<&Platform>,
        dir: &Path,
    ) -> Option<Dependency> {
        if d.git.is_some() {
            self.error(
                format!("git dependency `{key}` is not supported yet"),
                Some(span),
            );
            return None;
        }
        if let Some(r) = &d.registry {
            self.error(
                format!("dependency `{key}` uses registry `{r}`; only crates.io is supported"),
                Some(span),
            );
            return None;
        }
        let req = match d.version.as_deref().map(VersionReq::parse).transpose() {
            Ok(r) => r,
            Err(e) => {
                self.error(
                    format!("dependency `{key}`: invalid version: {e}"),
                    Some(span),
                );
                return None;
            }
        };
        let source = match (&d.path, req) {
            (Some(p), req) => {
                let abs = dir.join(p);
                match std::fs::canonicalize(&abs) {
                    Ok(c) => DepSource::Path(c, req),
                    Err(e) => {
                        self.error(
                            format!("path dependency `{key}`: {}: {e}", abs.display()),
                            Some(span),
                        );
                        return None;
                    }
                }
            }
            (None, Some(req)) => DepSource::Registry(req),
            (None, None) => {
                self.error(
                    format!("dependency `{key}` specifies neither `version` nor `path`"),
                    Some(span),
                );
                return None;
            }
        };
        Some(Dependency {
            name: key.to_string(),
            package: d.package.clone().unwrap_or_else(|| key.to_string()),
            kind,
            platform: platform.cloned(),
            optional: d.optional.unwrap_or(false),
            default_features: d.default_features.unwrap_or(true),
            features: d.features.clone().unwrap_or_default(),
            source,
            target: DepTarget::Unresolved("not yet mapped".into()),
            span: Some(span),
        })
    }

    fn lower_inherited_dep(
        &mut self,
        key: &str,
        member: &DetailedDependency,
        span: SourceSpan,
        kind: DepKind,
        platform: Option<&Platform>,
        edition: &str,
    ) -> Option<Dependency> {
        let Some(ws) = self.ws else {
            self.error(
                format!(
                    "`{key}.workspace = true` in a package outside the workspace is not supported"
                ),
                Some(span),
            );
            return None;
        };
        if member.version.is_some()
            || member.path.is_some()
            || member.git.is_some()
            || member.package.is_some()
            || member.registry.is_some()
        {
            self.error(
                format!(
                    "dependency `{key}` sets `workspace = true` together with a source key; \
                     only `features`, `optional` and `default-features` may be added"
                ),
                Some(span.clone()),
            );
        }
        let Some(ws_entry) = ws.dependencies.get(key) else {
            self.error(
                format!("dependency `{key}` sets `workspace = true`, but `[workspace.dependencies]` has no `{key}`"),
                Some(span),
            );
            return None;
        };
        let ws_span = ws.root_file.span(ws_entry.span());
        let ws_dep = match ws_entry.get_ref() {
            TomlDependency::Simple(v) => DetailedDependency {
                version: Some(v.clone()),
                ..Default::default()
            },
            TomlDependency::Detailed(d) => d.clone(),
        };
        if ws_dep.optional.is_some() {
            self.error(
                format!("`[workspace.dependencies]` entry `{key}` sets `optional`, which is only legal on the member side"),
                Some(ws_span.clone()),
            );
        }
        if ws_dep.workspace.is_some() {
            self.error(
                format!("`[workspace.dependencies]` entry `{key}` cannot set `workspace`"),
                Some(ws_span.clone()),
            );
            return None;
        }
        let root = ws
            .root_file
            .path
            .parent()
            .unwrap_or(Path::new(""))
            .to_path_buf();
        let mut dep = self.lower_plain_dep(key, &ws_dep, ws_span, kind, platform, &root)?;

        // Effective default-features follows Cargo: enabled unless the
        // workspace entry disables them; a member may re-enable them, but
        // a member `false` against enabled workspace defaults has no
        // effect (warning before edition 2024, error from 2024).
        let ws_default = ws_dep.default_features.unwrap_or(true);
        dep.default_features = ws_default || member.default_features == Some(true);
        if ws_default && member.default_features == Some(false) {
            let msg = format!(
                "dependency `{key}`: `default-features = false` is ignored because the \
                 `[workspace.dependencies]` entry enables default features"
            );
            if edition >= "2024" {
                self.error(msg, Some(span.clone()));
            } else {
                self.warnings
                    .push(Diagnostic::warning(msg).with_span(span.clone()));
            }
        }
        dep.optional = member.optional.unwrap_or(false);
        let mut features = dep.features.clone();
        features.extend(member.features.clone().unwrap_or_default());
        features.sort();
        features.dedup();
        dep.features = features;
        dep.span = Some(span);
        Some(dep)
    }

    /// `dropped_dev` names dev-dependencies left out of `deps`, which
    /// `dep/feature` entries may still name.
    fn lower_features(
        &mut self,
        m: &TomlManifest,
        deps: &[Dependency],
        dropped_dev: &BTreeSet<&str>,
    ) -> BTreeMap<String, Vec<FeatureEntry>> {
        let mut features: BTreeMap<String, Vec<FeatureEntry>> = BTreeMap::new();
        for (name, values) in &m.features {
            let entries = values
                .iter()
                .map(|v: &Spanned<String>| FeatureEntry {
                    value: FeatureValue::parse(v.get_ref()),
                    span: Some(self.file.span(v.span())),
                })
                .collect();
            features.insert(name.clone(), entries);
        }
        let dep_named: BTreeSet<String> = features
            .values()
            .flatten()
            .filter_map(|e| match &e.value {
                FeatureValue::Dep(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        let optional: BTreeSet<&str> = deps
            .iter()
            .filter(|d| d.optional)
            .map(|d| d.name.as_str())
            .collect();
        for &opt in &optional {
            if dep_named.contains(opt) {
                continue;
            }
            if features.contains_key(opt) {
                self.error(
                    format!(
                        "feature `{opt}` has the same name as an optional dependency; use \
                         `dep:{opt}` in the feature to activate the dependency"
                    ),
                    None,
                );
                continue;
            }
            features.insert(
                opt.to_string(),
                vec![FeatureEntry {
                    value: FeatureValue::Dep(opt.to_string()),
                    span: None,
                }],
            );
        }

        // Validate references the way Cargo does at manifest load.
        let dep_names: BTreeSet<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        let mut errors = Vec::new();
        for (fname, entries) in &features {
            for e in entries {
                let msg = match &e.value {
                    FeatureValue::Feature(f) if !features.contains_key(f) => Some(format!(
                        "feature `{fname}` includes `{f}`, which is neither a dependency nor \
                         another feature"
                    )),
                    FeatureValue::Dep(d) if !dep_names.contains(d.as_str()) => Some(format!(
                        "feature `{fname}` includes `dep:{d}`, but `{d}` is not a dependency"
                    )),
                    FeatureValue::Dep(d) if !optional.contains(d.as_str()) => Some(format!(
                        "feature `{fname}` includes `dep:{d}`, but `{d}` is not an optional \
                         dependency"
                    )),
                    FeatureValue::DepFeature { dep, .. }
                        if !dep_names.contains(dep.as_str())
                            && !dropped_dev.contains(dep.as_str()) =>
                    {
                        Some(format!(
                            "feature `{fname}` includes `{}`, but `{dep}` is not a dependency",
                            e.value
                        ))
                    }
                    FeatureValue::DepFeature {
                        dep, weak: true, ..
                    } if !optional.contains(dep.as_str()) => Some(format!(
                        "feature `{fname}` includes `{}` with a `?`, but `{dep}` is not an \
                         optional dependency",
                        e.value
                    )),
                    _ => None,
                };
                if let Some(msg) = msg {
                    errors.push((msg, e.span.clone()));
                }
            }
        }
        for (msg, span) in errors {
            self.error(msg, span);
        }
        features
    }

    fn lower_lints(&mut self, m: &TomlManifest) -> BTreeMap<String, BTreeMap<String, Lint>> {
        let Some(lints) = &m.lints else {
            return BTreeMap::new();
        };
        let tools = match lints.workspace {
            Some(true) => {
                if !lints.tools.is_empty() {
                    self.error(
                        "`[lints]` cannot combine `workspace = true` with its own lint tables",
                        None,
                    );
                }
                match self.ws {
                    Some(ws) => ws.lints.clone(),
                    None => BTreeMap::new(),
                }
            }
            Some(false) => {
                self.error("`lints.workspace` may only be `true`", None);
                return BTreeMap::new();
            }
            None => lints.tools.clone(),
        };
        tools
            .into_iter()
            .map(|(tool, lints)| {
                let lints = lints
                    .into_iter()
                    .map(|(name, l)| {
                        let lint = match l {
                            TomlLint::Level(level) => Lint {
                                level,
                                priority: 0,
                                check_cfg: Vec::new(),
                            },
                            TomlLint::Detailed {
                                level,
                                priority,
                                check_cfg,
                            } => Lint {
                                level,
                                priority,
                                check_cfg,
                            },
                        };
                        (name, lint)
                    })
                    .collect();
                (tool, lints)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `files` (path, contents) under a fresh tempdir.
    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let p = tmp.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        tmp
    }

    fn load(tmp: &tempfile::TempDir) -> Result<PackageSet> {
        load_workspace(tmp.path(), &RegistrySources::default())
    }

    const WS: &str = "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"b\"]\n\
                      [workspace.dependencies]\nb = { path = \"b\" }\n";
    const B: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\n";

    #[test]
    fn a_lone_package_is_its_own_workspace() {
        let tmp = tree(&[
            (
                "Cargo.toml",
                "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/main.rs", "fn main() {}\n"),
        ]);
        let set = load(&tmp).unwrap();
        let solo = &set.packages[set.member("solo").unwrap()];
        assert_eq!(solo.name(), "solo");
    }

    #[test]
    fn a_package_inside_a_workspace_must_load_from_its_root() {
        let tmp = tree(&[
            ("Cargo.toml", WS),
            (
                "a/Cargo.toml",
                "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
            ),
            ("a/src/lib.rs", ""),
            ("b/Cargo.toml", B),
            ("b/src/lib.rs", ""),
        ]);
        let err = load_workspace(&tmp.path().join("a"), &RegistrySources::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("point Gluon at the workspace root"), "{err}");
    }

    #[test]
    fn a_package_excluded_from_the_parent_workspace_loads_alone() {
        let tmp = tree(&[
            (
                "Cargo.toml",
                "[workspace]\nresolver = \"2\"\nmembers = []\nexclude = [\"solo\"]\n",
            ),
            (
                "solo/Cargo.toml",
                "[package]\nname = \"solo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("solo/src/lib.rs", ""),
        ]);
        let set = load_workspace(&tmp.path().join("solo"), &RegistrySources::default()).unwrap();
        assert!(set.member("solo").is_some());
    }

    #[test]
    fn tests_are_discovered_like_cargo() {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 [[test]]\nname = \"boot\"\nharness = false\n\
                 [[test]]\nname = \"custom\"\npath = \"checks/custom.rs\"\nharness = false\n";
        let tmp = tree(&[
            ("Cargo.toml", WS),
            ("a/Cargo.toml", a),
            ("a/src/lib.rs", ""),
            ("a/tests/boot.rs", ""),
            ("a/tests/smp/main.rs", ""),
            ("a/tests/notes.txt", ""),
            ("a/checks/custom.rs", ""),
            ("b/Cargo.toml", B),
            ("b/src/lib.rs", ""),
            ("b/tests/own.rs", ""),
        ]);
        let set = load(&tmp).unwrap();
        let a = &set.packages[set.member("a").unwrap()];
        let root = tmp.path().canonicalize().unwrap();
        let found: Vec<(&str, String, bool)> = a
            .tests
            .iter()
            .map(|t| {
                let path = t.path.strip_prefix(&root).unwrap();
                (t.name.as_str(), path.display().to_string(), t.harness)
            })
            .collect();
        assert_eq!(
            found,
            [
                ("boot", "a/tests/boot.rs".to_string(), false),
                ("custom", "a/checks/custom.rs".to_string(), false),
                ("smp", "a/tests/smp/main.rs".to_string(), true),
            ]
        );
    }

    #[test]
    fn autotests_false_keeps_only_declared_tests() {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nautotests = false\n\
                 [[test]]\nname = \"boot\"\nharness = false\n";
        let tmp = tree(&[
            ("Cargo.toml", WS),
            ("a/Cargo.toml", a),
            ("a/tests/boot.rs", ""),
            ("a/tests/other.rs", ""),
            ("b/Cargo.toml", B),
            ("b/src/lib.rs", ""),
        ]);
        let set = load(&tmp).unwrap();
        let a = &set.packages[set.member("a").unwrap()];
        let names: Vec<&str> = a.tests.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["boot"]);
    }

    #[test]
    fn declared_test_without_source_is_an_error() {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n[[test]]\nname = \"gone\"\n";
        let tmp = tree(&[
            ("Cargo.toml", WS),
            ("a/Cargo.toml", a),
            ("a/src/lib.rs", ""),
            ("b/Cargo.toml", B),
            ("b/src/lib.rs", ""),
        ]);
        let err = load(&tmp).unwrap_err().to_string();
        assert!(
            err.contains("cannot find the source of test `gone`"),
            "{err}"
        );
    }

    #[test]
    fn member_disabling_workspace_defaults_is_an_error_in_2024() {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                 [dependencies]\nb = { workspace = true, default-features = false }\n";
        let tmp = tree(&[
            ("Cargo.toml", WS),
            ("a/Cargo.toml", a),
            ("a/src/lib.rs", ""),
            ("b/Cargo.toml", B),
            ("b/src/lib.rs", ""),
        ]);
        let err = load(&tmp).unwrap_err().to_string();
        assert!(
            err.contains("`default-features = false` is ignored"),
            "{err}"
        );
    }

    #[test]
    fn dotted_dependency_keys_parse() {
        let ws = "[workspace]\nresolver = \"2\"\nmembers = [\"a\", \"b\"]\n\
                  [workspace.dependencies]\nb.path = \"b\"\n";
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\
                 [dependencies]\nb.workspace = true\n\
                 [target.'cfg(unix)'.dependencies]\nb.workspace = true\n";
        let tmp = tree(&[
            ("Cargo.toml", ws),
            ("a/Cargo.toml", a),
            ("a/src/lib.rs", ""),
            ("b/Cargo.toml", B),
            ("b/src/lib.rs", ""),
        ]);
        let set = load(&tmp).unwrap();
        let a = &set.packages[set.member("a").unwrap()];
        let b = set.member("b").unwrap();
        assert_eq!(a.deps.len(), 2);
        for d in &a.deps {
            assert_eq!(d.target, DepTarget::Resolved(b));
            let span = d.span.as_ref().expect("dotted entries carry a span");
            assert!(span.line > 0, "{span:?}");
        }
    }

    #[test]
    fn lints_workspace_true_cannot_add_lints() {
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n\
                 [lints]\nworkspace = true\n[lints.rust]\nunsafe_code = \"deny\"\n";
        let tmp = tree(&[
            ("Cargo.toml", WS),
            ("a/Cargo.toml", a),
            ("a/src/lib.rs", ""),
            ("b/Cargo.toml", B),
            ("b/src/lib.rs", ""),
        ]);
        let err = load(&tmp).unwrap_err().to_string();
        assert!(err.contains("cannot combine `workspace = true`"), "{err}");
    }

    #[test]
    fn stale_lock_leaves_edge_unresolved() {
        let ws = "[workspace]\nresolver = \"2\"\nmembers = [\"a\"]\n";
        let a = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n[dependencies]\nreg = \"2\"\n";
        let lock = "version = 4\n[[package]]\nname = \"a\"\nversion = \"0.1.0\"\n\
                    dependencies = [\"reg\"]\n[[package]]\nname = \"reg\"\nversion = \"1.0.0\"\n\
                    source = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        let tmp = tree(&[
            ("Cargo.toml", ws),
            ("Cargo.lock", lock),
            ("a/Cargo.toml", a),
            ("a/src/lib.rs", ""),
        ]);
        let set = load(&tmp).unwrap();
        let a = &set.packages[set.member("a").unwrap()];
        match &a.deps[0].target {
            DepTarget::Unresolved(r) => assert!(r.contains("requires `^2`"), "{r}"),
            t => panic!("expected unresolved, got {t:?}"),
        }
    }

    #[test]
    fn feature_values_parse() {
        assert_eq!(FeatureValue::parse("f"), FeatureValue::Feature("f".into()));
        assert_eq!(FeatureValue::parse("dep:x"), FeatureValue::Dep("x".into()));
        assert_eq!(
            FeatureValue::parse("x?/f"),
            FeatureValue::DepFeature {
                dep: "x".into(),
                feature: "f".into(),
                weak: true
            }
        );
        assert_eq!(FeatureValue::parse("x/f").to_string(), "x/f");
    }
}
